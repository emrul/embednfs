use super::*;

fn sample_entry(name: &str, fileid: u64) -> Entry4 {
    let mut bitmap = Bitmap4::new();
    bitmap.set(FATTR4_FILEID);
    bitmap.set(FATTR4_TYPE);

    let mut attr_vals = BytesMut::new();
    NfsFtype4::Reg.encode(&mut attr_vals);
    fileid.encode(&mut attr_vals);

    Entry4 {
        cookie: fileid,
        name: name.to_string(),
        attrs: Fattr4 {
            attrmask: bitmap,
            attr_vals: attr_vals.to_vec().into(),
        },
    }
}

#[test]
fn test_readdir_entry_len_matches_encoded_form() {
    let entry = sample_entry("hello.txt", 42);
    let mut encoded = BytesMut::new();
    entry.cookie.encode(&mut encoded);
    entry.name.encode(&mut encoded);
    entry.attrs.encode(&mut encoded);

    assert_eq!(readdir_entry_len(&entry), encoded.len());
    assert_eq!(readdir_entry_list_item_len(&entry), encoded.len() + 4);
    assert_eq!(
        readdir_dir_info_len(&entry),
        8 + xdr_opaque_len(entry.name.len())
    );
}

#[test]
fn test_readdir_resok_len_matches_readop_encoding() {
    let entries = vec![sample_entry("a.txt", 1), sample_entry("b.txt", 2)];
    let result = ReaddirRes4 {
        cookieverf: [1, 2, 3, 4, 5, 6, 7, 8],
        entries,
        eof: true,
    };

    let mut encoded = BytesMut::new();
    NfsResop4::Readdir(NfsStat4::Ok, Some(result)).encode(&mut encoded);

    let expected_entries = vec![sample_entry("a.txt", 1), sample_entry("b.txt", 2)];
    assert_eq!(
        readdir_resok_len(&expected_entries, true),
        encoded.len() - 8
    );
}

#[test]
fn test_synthetic_change_info_marks_response_non_atomic() {
    let cinfo = NfsServer::<crate::memfs::MemFs>::synthetic_change_info(41);
    assert!(!cinfo.atomic);
    assert_eq!(cinfo.before, 41);
    assert_eq!(cinfo.after, 42);
}

// ── Retirement fence ────────────────────────────────────────────────────────

mod retirement_fence {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test scaffolding: a failed unwrap/panic is the test failing"
    )]
    use crate::memfs::MemFs;
    use crate::server::NfsServer;
    use std::sync::Arc;

    fn server() -> NfsServer<MemFs> {
        NfsServer::new(MemFs::new())
    }

    /// Retirement is a fence, not just a snapshot: a handle that was retired
    /// can never be registered again, even though nothing about it is in the
    /// map any more.
    #[tokio::test]
    async fn a_retired_handle_cannot_be_registered_again() {
        let server = server();
        let control = server.control_handle();

        let id = server
            .register_handle(&7)
            .await
            .expect("first registration");
        assert!(server.register_handle(&7).await.is_ok(), "still live");

        let counts = control.retire_handles(|h: &u64| *h == 7).await;
        assert_eq!(counts.filehandles, 0, "no filehandle was ever issued");
        assert!(
            !server.handle_to_object.read().await.contains_key(&7),
            "the mapping must be dropped",
        );

        // The part a snapshot cannot express.
        assert!(
            server.register_handle(&7).await.is_err(),
            "a retired handle must never get an object id again",
        );
        // And an unrelated handle is unaffected.
        let other = server.register_handle(&8).await.expect("unrelated handle");
        assert_ne!(other, id);
    }

    /// The window a fence alone does not close, driven **deterministically**
    /// rather than by racing.
    ///
    /// `register_handle` returns an existing mapping without consulting the
    /// fence. So the state observable at the moment the fence is installed is
    /// what matters, and the test drives exactly that instant: install the
    /// fence via a retirement, then — with no concurrency at all — ask for the
    /// handle again. If the mapping were still present (as it was while the
    /// mappings were dropped only after the delegation recall, which can take
    /// as long as the recall deadline) this would hand back a live object id.
    #[tokio::test]
    async fn nothing_is_registrable_the_instant_the_fence_is_installed() {
        let server = server();
        let control = server.control_handle();

        let id = server.register_handle(&7).await.expect("registered");
        let object = crate::internal::ServerObject::Fs(id);
        let fh = server
            .state
            .object_to_fh(&object)
            .expect("a filehandle was issued");

        let _ = control.retire_handles(|h: &u64| *h == 7).await;

        // The mapping is gone, so the fast path cannot return it …
        assert!(
            server.register_handle(&7).await.is_err(),
            "the fast path must not return a retired mapping",
        );
        // … the old filehandle no longer resolves …
        assert_eq!(server.state.fh_to_object(&fh), None);
        // … and the already-resolved ObjectId cannot mint a new one.
        assert_eq!(
            server.state.object_to_fh(&object),
            None,
            "a resolved id must not mint a filehandle after the sweep",
        );
    }

    /// An `ObjectId` resolved before a retirement must not be able to create
    /// server state after it. The sweep runs once; these paths run later.
    #[tokio::test]
    async fn a_resolved_id_cannot_create_state_after_retirement() {
        let server = server();
        let control = server.control_handle();
        let id = server.register_handle(&7).await.expect("registered");
        let object = crate::internal::ServerObject::Fs(id);

        let _ = control.retire_handles(|h: &u64| *h == 7).await;

        assert!(
            server
                .state
                .create_open_state(object.clone(), 1, 1, 0)
                .await
                .is_err(),
            "an OPEN must not be created for a retired object",
        );
        assert!(
            server
                .state
                .create_directory_delegation(object, 1, None)
                .await
                .is_err(),
            "a delegation must not be granted for a retired object",
        );
    }

    /// The race the fence exists for: a lookup that resolved its handle before
    /// the withdrawal registers it after the retirement has taken its snapshot.
    ///
    /// The invariant is checked after the dust settles: *no* handle matching
    /// the retirement may hold a mapping. Without the fence, registrations that
    /// land after the snapshot survive it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_registration_racing_a_retirement_never_survives_it() {
        for round in 0..200u64 {
            let server = Arc::new(server());
            let control = server.control_handle();

            // Handles 0..8 are all retired; the racing registrations target
            // them, so every one of them must lose.
            let mut racers = Vec::new();
            for h in 0..8u64 {
                let server = Arc::clone(&server);
                racers.push(tokio::spawn(async move {
                    let _ = server.register_handle(&h).await;
                }));
            }

            let retire = tokio::spawn(async move {
                let _ = control.retire_handles(|h: &u64| *h < 8).await;
            });

            for r in racers {
                r.await.unwrap();
            }
            retire.await.unwrap();

            let map = server.handle_to_object.read().await;
            let survivors: Vec<u64> = map.keys().copied().filter(|h| *h < 8).collect();
            assert!(
                survivors.is_empty(),
                "round {round}: retired handles {survivors:?} outlived the retirement",
            );
        }
    }

    /// Retiring twice is safe, and the second fence does not undo the first.
    #[tokio::test]
    async fn retiring_twice_keeps_both_fences() {
        let server = server();
        let control = server.control_handle();

        let _ = server.register_handle(&1).await.unwrap();
        let _ = server.register_handle(&2).await.unwrap();

        let _ = control.retire_handles(|h: &u64| *h == 1).await;
        let _ = control.retire_handles(|h: &u64| *h == 2).await;

        assert!(server.register_handle(&1).await.is_err());
        assert!(server.register_handle(&2).await.is_err());
        assert!(server.register_handle(&3).await.is_ok());
        assert_eq!(server.retired.read().await.len(), 2, "both fences stand");
    }
}
