use super::*;

fn paths(n: usize) -> Vec<String> {
    (0..n).map(|i| std::env::temp_dir().join(format!("path {i} 日本語.pdf")).to_string_lossy().into_owned()).collect()
}

#[test]
fn a_request_names_the_paths_and_round_trips() {
    let paths = paths(2);
    let bytes = request(&paths, [1; 32], [2; 32]).unwrap();
    assert_eq!(parse_request(&bytes, &[1; 32]).unwrap(), (paths, [2; 32]));
    assert_eq!(parse_challenge(&challenge([1; 32]).unwrap()).unwrap(), [1; 32]);
    parse_acknowledgment(&acknowledgment([1; 32], [2; 32]).unwrap(), &[1; 32], &[2; 32]).unwrap();
}

#[test]
fn anything_but_a_versioned_document_request_is_refused() {
    for bytes in [b"".as_slice(), b"ok\n", b"{}", br#"{"open":[]}"#, br#"{"open":["a.pdf",3]}"#, br#"["a.pdf"]"#] {
        assert!(parse_request(bytes, &[1; 32]).is_err());
    }
    let original: serde_json::Value = serde_json::from_slice(&request(&paths(1), [1; 32], [2; 32]).unwrap()).unwrap();
    for (key, value) in [
        ("version", serde_json::json!(2)),
        ("version", serde_json::json!("1")),
        ("kind", serde_json::json!("control")),
        ("open", serde_json::json!([])),
        ("open", serde_json::json!(["relative.pdf"])),
        ("open", serde_json::json!(["\u{0}"])),
        ("client", serde_json::json!([1, 2])),
        ("server", serde_json::json!(vec![3; 32])),
        ("command", serde_json::json!("ui.set")),
    ] {
        let mut value_json = original.clone();
        value_json[key] = value;
        assert!(parse_request(&serde_json::to_vec(&value_json).unwrap(), &[1; 32]).is_err(), "{key}");
    }
    assert!(parse_request(&challenge([1; 32]).unwrap(), &[1; 32]).is_err(), "no reflected challenge");
}

#[test]
fn replies_and_requests_are_bound_to_this_exchange() {
    assert!(parse_request(&request(&paths(1), [1; 32], [2; 32]).unwrap(), &[3; 32]).is_err());
    let ack = acknowledgment([1; 32], [2; 32]).unwrap();
    assert!(parse_acknowledgment(&ack, &[3; 32], &[2; 32]).is_err());
    assert!(parse_acknowledgment(&ack, &[1; 32], &[3; 32]).is_err());
    assert!(parse_acknowledgment(b"ok\n", &[1; 32], &[2; 32]).is_err());
}

#[test]
fn path_count_and_byte_limits_apply_before_sending_or_queueing() {
    assert!(request(&paths(MAX_PATHS + 1), [1; 32], [2; 32]).is_err());
    let long = std::env::temp_dir().join("x".repeat(MAX_PATH_BYTES)).to_string_lossy().into_owned();
    assert!(request(&[long], [1; 32], [2; 32]).is_err());
    assert!(object(&vec![b' '; MAX_REQUEST + 1], "open", &[]).is_err());
}

#[test]
fn framing_rejects_oversized_empty_and_truncated_messages() {
    let mut bytes = Vec::new();
    write_frame(&mut bytes, b"payload", 7).unwrap();
    assert_eq!(read_frame(&mut bytes.as_slice(), 7).unwrap(), b"payload");
    for len in 0..bytes.len() {
        assert!(read_frame(&mut &bytes[..len], 7).is_err(), "truncated at {len}");
    }
    assert!(read_frame(&mut 8u32.to_be_bytes().as_slice(), 7).is_err());
    assert!(read_frame(&mut 0u32.to_be_bytes().as_slice(), 7).is_err());
    assert!(write_frame(&mut Vec::new(), b"too long", 7).is_err());
    assert!(write_frame(&mut Vec::new(), b"", 7).is_err());
}

#[test]
fn relative_paths_are_made_absolute_against_this_launch() {
    assert_eq!(absolute(&["Quote.pdf".to_string()]), vec![std::env::current_dir().unwrap().join("Quote.pdf").to_string_lossy().into_owned()]);
    let already = paths(1).remove(0);
    assert_eq!(absolute(std::slice::from_ref(&already)), vec![already]);
}

#[test]
fn each_windows_session_and_launch_kind_gets_its_own_socket_name() {
    assert_ne!(socket_name(3, false), socket_name(4, false));
    assert_ne!(socket_name(3, false), socket_name(3, true));
    assert_eq!(socket_name(0, false), "instance-0.sock");
    assert_eq!(socket_name(u32::MAX, true), "portable-instance-4294967295.sock");
}

#[test]
fn options_including_control_and_image_import_keep_an_independent_window() {
    let files = paths(1);
    assert!(may_hand_off(false, &files, false, false, false));
    for (new, options, control, images) in
        [(true, false, false, false), (false, true, false, false), (false, false, true, false), (false, false, false, true)]
    {
        assert!(!may_hand_off(new, &files, options, control, images));
    }
    assert!(!may_hand_off(false, &[], false, false, false));
    #[cfg(not(windows))]
    assert!(matches!(claim(&files, true), Claim::Alone));
}

#[test]
fn delivered_files_reach_the_ui_once_wake_it_and_bring_the_window_forward() {
    let server = Server::new();
    let files = paths(2);
    server.deliver(vec![files[0].clone()]).unwrap();
    let mut poll = server.connect(&egui::Context::default());
    server.deliver(vec![files[1].clone()]).unwrap();
    assert!(matches!(poll().as_slice(), [OsEvent::Open(p)] if p == &files));
    assert!(poll().is_empty());
    server.deliver(paths(1)).unwrap();
    assert!(!poll().is_empty(), "draining restores capacity");
}

#[test]
fn queue_capacity_rejects_the_whole_batch_and_draining_restores_capacity() {
    let server = Server::new();
    for _ in 0..MAX_BATCHES {
        server.deliver(paths(1)).unwrap();
    }
    assert_eq!(server.deliver(paths(2)).unwrap_err().kind(), io::ErrorKind::WouldBlock);
    let mut poll = server.connect(&egui::Context::default());
    assert!(matches!(poll().as_slice(), [OsEvent::Open(p)] if p.len() == MAX_BATCHES));
    for _ in 0..MAX_QUEUED_PATHS / MAX_PATHS {
        server.deliver(paths(MAX_PATHS)).unwrap();
    }
    assert!(server.deliver(paths(1)).is_err());
    poll();
    // The byte bound is independent of the path/batch bounds.
    let file = std::env::temp_dir().join("x".repeat(20_000)).to_string_lossy().into_owned();
    for _ in 0..4 {
        server.deliver(vec![file.clone(); 50]).unwrap();
    }
    assert!(server.deliver(vec![file; 50]).is_err());
}

#[test]
fn concurrent_clients_cannot_reserve_more_than_the_limit() {
    let count = Arc::<AtomicUsize>::default();
    let permits: Vec<_> = (0..MAX_CONNECTIONS).map(|_| Permit::acquire(&count).unwrap()).collect();
    assert!(Permit::acquire(&count).is_none());
    drop(permits);
    assert_eq!(count.load(Ordering::Acquire), 0);
    assert!(Permit::acquire(&count).is_some());
}

struct Trickle;
impl Read for Trickle {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        std::thread::sleep(Duration::from_millis(5));
        if let Some(byte) = bytes.first_mut() {
            *byte = 0;
        }
        Ok(1)
    }
}

#[test]
fn trickled_bytes_cannot_renew_the_total_deadline_and_stop_interrupts_io() {
    let start = Instant::now();
    let mut io = DeadlineIo { inner: Trickle, deadline: start + Duration::from_millis(25), stop: Arc::default() };
    let mut bytes = [0; 100];
    assert_eq!(io.read_exact(&mut bytes).unwrap_err().kind(), io::ErrorKind::TimedOut);
    assert!(start.elapsed() < Duration::from_secs(1));
    io.stop.store(true, Ordering::Release);
    assert_eq!(io.read(&mut bytes).unwrap_err().kind(), io::ErrorKind::TimedOut);
}

#[cfg(windows)]
mod native {
    use super::*;
    use uds_windows::{UnixListener, UnixStream};

    fn socket(test: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pdfcraft-instance-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(socket_name(0, false))
    }

    fn poll_until(poll: &mut OsEventsFn, expected: usize) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut files = Vec::new();
        while files.len() < expected && Instant::now() < deadline {
            for event in poll() {
                let OsEvent::Open(paths) = event else { panic!("only document opens") };
                files.extend(paths);
            }
            std::thread::sleep(POLL);
        }
        assert_eq!(files.len(), expected);
        files
    }

    fn hello(stream: &UnixStream) -> (security::Protection, Nonce) {
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        stream.set_write_timeout(Some(TIMEOUT)).unwrap();
        let crypto = security::Protection::new().unwrap();
        let sealed = read_frame(&mut &*stream, MAX_HELLO).unwrap();
        let server = parse_challenge(&crypto.unprotect(&sealed, Instant::now() + TIMEOUT, &AtomicBool::new(false)).unwrap()).unwrap();
        (crypto, server)
    }

    #[test]
    fn windows_user_protection_round_trips_and_detects_tampering() {
        let crypto = security::Protection::new().unwrap();
        let stop = AtomicBool::new(false);
        let deadline = Instant::now() + TIMEOUT;
        let plaintext = request(&paths(1), [1; 32], crypto.nonce().unwrap()).unwrap();
        let sealed = crypto.protect(&plaintext, deadline, &stop).unwrap();
        assert_ne!(sealed, plaintext);
        assert_eq!(crypto.unprotect(&sealed, deadline, &stop).unwrap(), plaintext);
        assert!(crypto.unprotect(b"not protected data", deadline, &stop).is_err());
        let mut tampered = sealed;
        let last = tampered.last_mut().unwrap();
        *last ^= 0xff;
        assert!(crypto.unprotect(&tampered, deadline, &stop).is_err());
        assert!(crypto.protect(&plaintext, Instant::now(), &stop).is_err());
    }

    #[test]
    fn a_regular_file_at_the_endpoint_is_never_removed() {
        let path = socket("regular-endpoint");
        std::fs::write(&path, b"unrelated data").unwrap();
        assert!(matches!(claim_at(&path, &paths(1), true), Claim::Alone));
        assert_eq!(std::fs::read(&path).unwrap(), b"unrelated data");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn an_idle_client_expires_and_does_not_hold_a_receiver_forever() {
        let (server_stream, _client_stream) = UnixStream::pair().unwrap();
        let start = Instant::now();
        let inbox = Mutex::<Inbox>::default();
        let result = receive(&server_stream, &inbox, start + Duration::from_millis(100), Arc::default());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(inbox.lock().unwrap().paths.is_empty());
    }

    #[test]
    fn a_second_launch_hands_its_files_to_the_first() {
        let path = socket("hand-off");
        let Claim::Primary(first) = claim_at(&path, &[], false) else { panic!("the first launch listens") };
        let mut poll = first.connect(&egui::Context::default());
        let files = paths(2);
        assert!(matches!(claim_at(&path, &files, true), Claim::HandedOff));
        assert_eq!(poll_until(&mut poll, 2), files);
        assert!(matches!(claim_at(&path, &paths(1), false), Claim::Alone));
        assert!(poll().is_empty());
        drop(first);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn launches_started_together_share_one_window() {
        let path = socket("together");
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let launches: Vec<_> = (0..8)
            .map(|i| {
                let (path, barrier) = (path.clone(), Arc::clone(&barrier));
                std::thread::spawn(move || {
                    barrier.wait();
                    claim_at(&path, &[paths(8).remove(i)], true)
                })
            })
            .collect();
        let claims: Vec<Claim> = launches.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(claims.iter().filter(|c| matches!(c, Claim::Primary(_))).count(), 1);
        assert_eq!(claims.iter().filter(|c| matches!(c, Claim::HandedOff)).count(), 7);
        drop(claims);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_socket_left_by_a_crashed_instance_is_replaced() {
        let path = socket("stale");
        drop(UnixListener::bind(&path).unwrap());
        assert!(std::fs::symlink_metadata(&path).is_ok());
        let Claim::Primary(server) = claim_at(&path, &paths(1), true) else { panic!("stale socket replaced") };
        drop(server);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn malformed_oversized_truncated_and_replayed_requests_do_not_queue_or_stop_the_primary() {
        let path = socket("hostile");
        let Claim::Primary(server) = claim_at(&path, &[], false) else { panic!("primary") };
        let mut poll = server.connect(&egui::Context::default());
        for case in 0..6 {
            let stream = UnixStream::connect(&path).unwrap();
            let (crypto, nonce) = hello(&stream);
            match case {
                0 => write_frame(&mut &stream, b"unprotected", MAX_FRAME).unwrap(),
                1 => (&stream).write_all(&((MAX_FRAME + 1) as u32).to_be_bytes()).unwrap(),
                2 => {
                    (&stream).write_all(&100u32.to_be_bytes()).unwrap();
                    (&stream).write_all(b"truncated").unwrap();
                    stream.shutdown(std::net::Shutdown::Write).unwrap();
                }
                _ => {
                    let mut body: serde_json::Value = serde_json::from_slice(&request(&paths(1), nonce, [2; 32]).unwrap()).unwrap();
                    match case {
                        3 => body["version"] = serde_json::json!(999),
                        4 => body["command"] = serde_json::json!("ui.set"),
                        _ => body["server"] = serde_json::json!(vec![0; 32]),
                    }
                    let sealed = crypto.protect(&serde_json::to_vec(&body).unwrap(), Instant::now() + TIMEOUT, &AtomicBool::new(false)).unwrap();
                    write_frame(&mut &stream, &sealed, MAX_FRAME).unwrap();
                }
            }
            let mut byte = [0];
            assert!(matches!((&stream).read(&mut byte), Ok(0) | Err(_)), "request must be rejected");
            assert!(poll().is_empty());
        }
        assert!(matches!(claim_at(&path, &paths(1), true), Claim::HandedOff));
        poll_until(&mut poll, 1);
        drop(server);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_stalled_connection_does_not_block_valid_launches_and_shutdown_is_bounded() {
        let path = socket("stalled");
        let Claim::Primary(server) = claim_at(&path, &[], false) else { panic!("primary") };
        let stream = UnixStream::connect(&path).unwrap();
        hello(&stream);
        assert!(matches!(claim_at(&path, &paths(1), true), Claim::HandedOff));
        let start = Instant::now();
        drop(server);
        assert!(start.elapsed() < Duration::from_secs(2), "stop interrupts stalled reads");
        drop(stream);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn full_queue_rejects_handoff_and_retains_graceful_window_fallback() {
        let path = socket("full");
        let Claim::Primary(server) = claim_at(&path, &[], false) else { panic!("primary") };
        for _ in 0..MAX_BATCHES {
            server.deliver(paths(1)).unwrap();
        }
        assert!(matches!(claim_at(&path, &paths(1), true), Claim::Alone));
        let mut poll = server.connect(&egui::Context::default());
        assert_eq!(poll_until(&mut poll, MAX_BATCHES).len(), MAX_BATCHES);
        assert!(matches!(claim_at(&path, &paths(1), true), Claim::HandedOff));
        drop(server);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn acknowledgment_loss_can_duplicate_opens_with_the_preserved_fallback_policy() {
        let path = socket("ack-loss");
        let Claim::Primary(server) = claim_at(&path, &[], false) else { panic!("primary") };
        let mut poll = server.connect(&egui::Context::default());
        let stream = UnixStream::connect(&path).unwrap();
        let (crypto, nonce) = hello(&stream);
        let files = paths(1);
        let sealed = crypto.protect(&request(&files, nonce, [7; 32]).unwrap(), Instant::now() + TIMEOUT, &AtomicBool::new(false)).unwrap();
        write_frame(&mut &stream, &sealed, MAX_FRAME).unwrap();
        stream.shutdown(std::net::Shutdown::Both).unwrap();
        drop(stream);
        assert_eq!(poll_until(&mut poll, 1), files);
        // We make no exactly-once promise across uncertain ACKs and fallback launches.
        assert!(matches!(claim_at(&path, &files, true), Claim::HandedOff));
        assert_eq!(poll_until(&mut poll, 1), files);
        drop(server);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_replaced_listener_cannot_confirm_with_an_unbound_response() {
        let path = socket("impersonator");
        let listener = UnixListener::bind(&path).unwrap();
        let impostor = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let crypto = security::Protection::new().unwrap();
            let stop = AtomicBool::new(false);
            let deadline = Instant::now() + TIMEOUT;
            write_frame(&mut &stream, &crypto.protect(&challenge([9; 32]).unwrap(), deadline, &stop).unwrap(), MAX_HELLO).unwrap();
            let _request = read_frame(&mut &stream, MAX_FRAME).unwrap();
            // Same-user test process deliberately ignores the sealed request instead of
            // unprotecting it. A different user cannot retrieve its client nonce.
            write_frame(&mut &stream, &crypto.protect(&acknowledgment([9; 32], [0; 32]).unwrap(), deadline, &stop).unwrap(), MAX_HELLO).unwrap();
        });
        assert!(matches!(claim_at(&path, &paths(1), true), Claim::Alone));
        impostor.join().unwrap();
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    fn child(path: &std::path::Path, mode: &str, index: usize) -> std::process::Child {
        let output = std::fs::File::create(path.parent().unwrap().join(format!("child-{index}.log"))).unwrap();
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "single_instance::tests::native::process_child", "--ignored", "--nocapture"])
            .env("PDFCRAFT_IPC_TEST_PATH", path)
            .env("PDFCRAFT_IPC_TEST_MODE", mode)
            .env("PDFCRAFT_IPC_TEST_INDEX", index.to_string())
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap()
    }

    fn wait_file(path: &std::path::Path) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !path.exists() && Instant::now() < deadline {
            std::thread::sleep(POLL);
        }
        assert!(path.exists(), "{}", path.display());
    }

    #[test]
    #[ignore = "helper invoked by real-process IPC tests"]
    fn process_child() {
        let path = std::path::PathBuf::from(std::env::var_os("PDFCRAFT_IPC_TEST_PATH").unwrap());
        let mode = std::env::var("PDFCRAFT_IPC_TEST_MODE").unwrap();
        let index: usize = std::env::var("PDFCRAFT_IPC_TEST_INDEX").unwrap().parse().unwrap();
        let dir = path.parent().unwrap();
        if mode == "auto-activation" {
            // Control experiment: safe windows bindings maintain their process MTA
            // when activation begins without a caller-owned, temporary COM guard.
            use windows::Security::Cryptography::DataProtection::DataProtectionProvider;
            for _ in 0..3 {
                let encrypt = DataProtectionProvider::CreateOverloadExplicit(&"LOCAL=user".into()).unwrap();
                let decrypt = DataProtectionProvider::new().unwrap();
                drop((encrypt, decrypt));
            }
            return;
        }
        if mode == "crypto-cycle" {
            // A cold process has no other test's COM apartment keeping factories alive.
            // Deliberately isolate activation/teardown from sockets and election.
            for _ in 0..3 {
                let crypto = security::Protection::new().unwrap();
                let stop = AtomicBool::new(false);
                let deadline = Instant::now() + TIMEOUT;
                let sealed = crypto.protect(b"cold process", deadline, &stop).unwrap();
                assert_eq!(crypto.unprotect(&sealed, deadline, &stop).unwrap(), b"cold process");
                drop(crypto);
            }
            return;
        }
        if mode == "profile" {
            let endpoint = socket_path().unwrap();
            std::fs::write(dir.join("profile-result"), endpoint.to_string_lossy().as_bytes()).unwrap();
            return;
        }
        if mode == "lock" {
            let _lock = election(&path).unwrap();
            std::fs::write(dir.join("ready"), b"ready").unwrap();
            std::thread::sleep(Duration::from_secs(30));
            return;
        }
        if mode == "race" {
            wait_file(&dir.join("go"));
        }
        let claim = claim_at(&path, &[paths(8).remove(index)], true);
        let role = match &claim {
            Claim::Primary(_) => "primary",
            Claim::HandedOff => "forwarded",
            Claim::Alone => "alone",
        };
        // Publish the complete role atomically: existence must not expose a file
        // between creation and writing while the parent waits for election results.
        let role_path = dir.join(format!("role-{index}"));
        let pending = role_path.with_extension("pending");
        std::fs::write(&pending, role).unwrap();
        std::fs::rename(pending, role_path).unwrap();
        if let Claim::Primary(server) = claim {
            std::fs::write(dir.join("ready"), b"ready").unwrap();
            wait_file(&dir.join("release"));
            let mut poll = server.connect(&egui::Context::default());
            if mode == "race" {
                std::fs::write(dir.join("received"), serde_json::to_vec(&poll_until(&mut poll, 7)).unwrap()).unwrap();
            }
        }
    }

    #[test]
    fn windows_bindings_automatic_activation_survives_cold_process_cycles() {
        let path = socket("auto-activation");
        let mut child = child(&path, "auto-activation", 0);
        let status = child.wait().unwrap();
        assert!(
            status.success(),
            "automatic activation child exited with {status}\n{}",
            std::fs::read_to_string(path.parent().unwrap().join("child-0.log")).unwrap()
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn user_protection_survives_repeated_cold_process_activation() {
        let path = socket("crypto-cycle");
        let mut child = child(&path, "crypto-cycle", 0);
        let status = child.wait().unwrap();
        assert!(
            status.success(),
            "cold crypto child exited with {status}\n{}",
            std::fs::read_to_string(path.parent().unwrap().join("child-0.log")).unwrap()
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn real_process_startup_races_forward_every_secondary_batch() {
        let path = socket("process-race");
        let dir = path.parent().unwrap();
        let mut children: Vec<_> = (0..8).map(|i| child(&path, "race", i)).collect();
        std::fs::write(dir.join("go"), b"go").unwrap();
        let roles: Vec<_> = (0..8)
            .map(|i| {
                let role = dir.join(format!("role-{i}"));
                wait_file(&role);
                std::fs::read_to_string(role).unwrap()
            })
            .collect();
        std::fs::write(dir.join("release"), b"release").unwrap();
        let outcomes: Vec<_> = children.iter_mut().enumerate().map(|(index, child)| (index, child.wait().unwrap())).collect();
        let evidence = outcomes
            .iter()
            .map(|(index, status)| {
                format!("child {index} exited with {status}\n{}", std::fs::read_to_string(dir.join(format!("child-{index}.log"))).unwrap())
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(outcomes.iter().all(|(_, status)| status.success()), "roles: {roles:?}\n{evidence}");
        assert_eq!(roles.iter().filter(|r| *r == "primary").count(), 1);
        assert_eq!(roles.iter().filter(|r| *r == "forwarded").count(), 7);
        let received: Vec<String> = serde_json::from_slice(&std::fs::read(dir.join("received")).unwrap()).unwrap();
        assert_eq!(received.len(), 7);
        let unique: std::collections::HashSet<_> = received.iter().collect();
        assert_eq!(unique.len(), 7);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn endpoint_identity_comes_from_windows_despite_spoofed_environment() {
        let path = socket("environment");
        let dir = path.parent().unwrap();
        let mut process = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "single_instance::tests::native::process_child", "--ignored"])
            .env("PDFCRAFT_IPC_TEST_PATH", &path)
            .env("PDFCRAFT_IPC_TEST_MODE", "profile")
            .env("PDFCRAFT_IPC_TEST_INDEX", "0")
            .env("LOCALAPPDATA", dir.join("shared-folder"))
            .env("SESSIONNAME", "spoofed-session")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        assert!(process.wait().unwrap().success());
        assert_eq!(std::fs::read_to_string(dir.join("profile-result")).unwrap(), socket_path().unwrap().to_string_lossy());
        assert!(!dir.join("shared-folder").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn killed_primary_and_election_holder_release_os_resources() {
        for mode in ["primary", "lock"] {
            let path = socket(mode);
            let mut process = child(&path, mode, 0);
            wait_file(&path.parent().unwrap().join("ready"));
            process.kill().unwrap();
            process.wait().unwrap();
            let Claim::Primary(server) = claim_at(&path, &paths(1), true) else { panic!("recover after {mode} crash") };
            drop(server);
            std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
    }
}
