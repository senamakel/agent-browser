//! Integration tests for the standalone dashboard lifecycle.

use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const BIN: &str = env!("CARGO_BIN_EXE_agent-browser");
const DASHBOARD_DIAGNOSTIC_ENV: &str = "AGENT_BROWSER_DASHBOARD_DIAGNOSTICS";
const LAUNCHER_EXIT_TIMEOUT: Duration = Duration::from_secs(8);
const PIPE_EOF_TIMEOUT: Duration = Duration::from_secs(2);
const WORKER_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

static NEXT_DASHBOARD_DIAGNOSTIC_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static DASHBOARD_DIAGNOSTIC_INCOMPLETE: Cell<bool> = const { Cell::new(false) };
    static DASHBOARD_DIAGNOSTIC_PENDING_PIPES: RefCell<Vec<PendingDashboardPipe>> = const { RefCell::new(Vec::new()) };
}

struct PendingDashboardPipe {
    invocation: u64,
    operation: &'static str,
    stream: &'static str,
    receiver: Receiver<std::io::Result<Vec<u8>>>,
}

struct DashboardDiagnosticCheck;

impl DashboardDiagnosticCheck {
    fn new() -> Self {
        if dashboard_diagnostics_enabled() {
            DASHBOARD_DIAGNOSTIC_INCOMPLETE.with(|incomplete| incomplete.set(false));
        }
        Self
    }
}

impl Drop for DashboardDiagnosticCheck {
    fn drop(&mut self) {
        if dashboard_diagnostics_enabled() && !thread::panicking() {
            let incomplete = DASHBOARD_DIAGNOSTIC_INCOMPLETE.with(Cell::get);
            assert!(!incomplete, "dashboard diagnostic capture was incomplete");
        }
    }
}

struct DashboardCleanup<'a>(&'a TempDir);

impl Drop for DashboardCleanup<'_> {
    fn drop(&mut self) {
        let _ = run_dashboard(self.0, &["dashboard", "stop", "--json"]);
        if dashboard_diagnostics_enabled() {
            observe_pending_pipes_after_worker_stop();
        }
    }
}

struct RunningDashboard(Child);

impl Drop for RunningDashboard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        if dashboard_diagnostics_enabled() {
            observe_pending_pipes_after_worker_stop();
        }
    }
}

fn socket_dir(tmp: &TempDir) -> std::path::PathBuf {
    tmp.path().join("sockets")
}

fn unused_loopback_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_for_dashboard(address: SocketAddr, host: &str, origin: &str) -> String {
    let diagnostics = dashboard_diagnostics_enabled();
    let invocation = if diagnostics {
        NEXT_DASHBOARD_DIAGNOSTIC_ID.fetch_add(1, Ordering::SeqCst)
    } else {
        0
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let connect_started = Instant::now();
        match TcpStream::connect_timeout(&address, Duration::from_millis(100)) {
            Ok(mut stream) => {
                if diagnostics {
                    dashboard_diagnostic_marker(
                        invocation,
                        "start",
                        "loopback_connect",
                        connect_started,
                        "connected",
                        "address_omitted",
                    );
                    stream
                        .set_write_timeout(Some(Duration::from_secs(2)))
                        .expect("set diagnostic dashboard write timeout");
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .expect("set diagnostic dashboard read timeout");
                }
                let write_started = Instant::now();
                let request = format!(
                    "GET /api/sessions HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nConnection: close\r\n\r\n"
                );
                if let Err(error) = stream.write_all(request.as_bytes()) {
                    if diagnostics {
                        dashboard_diagnostic_marker(
                            invocation,
                            "start",
                            "request_write",
                            write_started,
                            "error",
                            "bytes_omitted",
                        );
                    }
                    panic!("dashboard probe request write failed: {error}");
                }
                if diagnostics {
                    dashboard_diagnostic_marker(
                        invocation,
                        "start",
                        "request_write",
                        write_started,
                        "ok",
                        "bytes_omitted",
                    );
                }
                let mut response = String::new();
                let read_started = Instant::now();
                if let Err(error) = stream.read_to_string(&mut response) {
                    if diagnostics {
                        dashboard_diagnostic_marker(
                            invocation,
                            "start",
                            "response_read",
                            read_started,
                            "error",
                            &format!("bytes={}", response.len()),
                        );
                        panic!("dashboard probe response read failed");
                    }
                    panic!("dashboard probe response read failed: {error}");
                }
                if diagnostics {
                    dashboard_diagnostic_marker(
                        invocation,
                        "start",
                        "response_read",
                        read_started,
                        "eof",
                        &format!("bytes={}", response.len()),
                    );
                }
                return response;
            }
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => panic!("dashboard did not accept {address}: {error}"),
        }
    }
}

fn seed_running_dashboard(tmp: &TempDir, port: u16, allowed_origins: &[&str]) -> RunningDashboard {
    let socket_dir = socket_dir(tmp);
    std::fs::create_dir_all(&socket_dir).unwrap();
    let child = Command::new(BIN)
        .env("AGENT_BROWSER_DASHBOARD", "1")
        .env("AGENT_BROWSER_DASHBOARD_PORT", "0")
        .env("AGENT_BROWSER_SOCKET_DIR", &socket_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to seed a running dashboard process");
    let config = serde_json::json!({
        "port": port,
        "allowed_origins": allowed_origins,
        "access_token": "a".repeat(64),
    });
    std::fs::write(
        socket_dir.join("dashboard.config"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    std::fs::write(socket_dir.join("dashboard.pid"), child.id().to_string()).unwrap();
    RunningDashboard(child)
}

fn run_dashboard(tmp: &TempDir, args: &[&str]) -> Output {
    let socket_dir = socket_dir(tmp);
    std::fs::create_dir_all(&socket_dir).unwrap();

    let mut command = Command::new(BIN);
    command
        .args(args)
        .env("AGENT_BROWSER_SOCKET_DIR", &socket_dir)
        .env_remove("AGENT_BROWSER_DASHBOARD_ALLOWED_ORIGINS")
        .env("NO_COLOR", "1");

    if dashboard_diagnostics_enabled() {
        return run_dashboard_with_diagnostics(command, &socket_dir, args);
    }

    command
        .output()
        .expect("failed to invoke agent-browser dashboard")
}

fn dashboard_diagnostics_enabled() -> bool {
    std::env::var_os(DASHBOARD_DIAGNOSTIC_ENV).is_some()
}

fn dashboard_test_output(bytes: &[u8]) -> String {
    if dashboard_diagnostics_enabled() {
        "<redacted>".to_string()
    } else {
        String::from_utf8_lossy(bytes).to_string()
    }
}

fn dashboard_operation(args: &[&str]) -> &'static str {
    if args.first() == Some(&"dashboard") && args.get(1) == Some(&"stop") {
        "stop"
    } else if args.first() == Some(&"dashboard") {
        "start"
    } else {
        "other"
    }
}

fn dashboard_diagnostic_marker(
    invocation: u64,
    operation: &str,
    stage: &str,
    started: Instant,
    outcome: &str,
    detail: &str,
) {
    eprintln!(
        "DASHBOARD_DIAG invocation={invocation} operation={operation} stage={stage} elapsed_ms={} outcome={outcome} {detail}",
        started.elapsed().as_millis()
    );
}

fn dashboard_probe_port(
    operation: &str,
    args: &[&str],
    socket_dir: &std::path::Path,
) -> Option<u16> {
    if operation == "start" {
        return args
            .windows(2)
            .find(|pair| pair[0] == "--port")
            .and_then(|pair| pair[1].parse::<u16>().ok())
            .or(Some(4848));
    }
    if operation == "stop" {
        return std::fs::read(socket_dir.join("dashboard.config"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|config| config.get("port")?.as_u64())
            .and_then(|port| u16::try_from(port).ok());
    }
    None
}

fn dashboard_worker_is_listening(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if TcpStream::connect_timeout(
            &SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            Duration::from_millis(100),
        )
        .is_ok()
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn read_pipe_to_channel<R: Read + Send + 'static>(
    mut pipe: R,
    sender: mpsc::Sender<std::io::Result<Vec<u8>>>,
) {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = pipe.read_to_end(&mut bytes).map(|_| bytes);
        let _ = sender.send(result);
    });
}

fn retain_pending_pipe(
    receiver: Receiver<std::io::Result<Vec<u8>>>,
    invocation: u64,
    operation: &'static str,
    stream: &'static str,
) {
    DASHBOARD_DIAGNOSTIC_PENDING_PIPES.with(|pending| {
        pending.borrow_mut().push(PendingDashboardPipe {
            invocation,
            operation,
            stream,
            receiver,
        });
    });
}

fn observe_pending_pipes_after_worker_stop() {
    let pending = DASHBOARD_DIAGNOSTIC_PENDING_PIPES
        .with(|pending| std::mem::take(&mut *pending.borrow_mut()));
    for pipe in pending {
        let started = Instant::now();
        match pipe.receiver.recv_timeout(PIPE_EOF_TIMEOUT) {
            Ok(Ok(bytes)) => dashboard_diagnostic_marker(
                pipe.invocation,
                pipe.operation,
                match pipe.stream {
                    "stdout" => "stdout_eof_after_worker_stop",
                    _ => "stderr_eof_after_worker_stop",
                },
                started,
                "eof",
                &format!("bytes={}", bytes.len()),
            ),
            Ok(Err(_)) => dashboard_diagnostic_marker(
                pipe.invocation,
                pipe.operation,
                match pipe.stream {
                    "stdout" => "stdout_eof_after_worker_stop",
                    _ => "stderr_eof_after_worker_stop",
                },
                started,
                "read_error",
                "bytes=0",
            ),
            Err(mpsc::RecvTimeoutError::Timeout) => dashboard_diagnostic_marker(
                pipe.invocation,
                pipe.operation,
                match pipe.stream {
                    "stdout" => "stdout_eof_after_worker_stop",
                    _ => "stderr_eof_after_worker_stop",
                },
                started,
                "timeout",
                "bytes=0",
            ),
            Err(mpsc::RecvTimeoutError::Disconnected) => dashboard_diagnostic_marker(
                pipe.invocation,
                pipe.operation,
                match pipe.stream {
                    "stdout" => "stdout_eof_after_worker_stop",
                    _ => "stderr_eof_after_worker_stop",
                },
                started,
                "disconnected",
                "bytes=0",
            ),
        }
    }
}

fn collect_diagnostic_pipe(
    receiver: Receiver<std::io::Result<Vec<u8>>>,
    invocation: u64,
    operation: &'static str,
    name: &'static str,
) -> (Vec<u8>, bool) {
    let started = Instant::now();
    match receiver.recv_timeout(PIPE_EOF_TIMEOUT) {
        Ok(Ok(bytes)) => {
            dashboard_diagnostic_marker(
                invocation,
                operation,
                name,
                started,
                "eof",
                &format!("bytes={}", bytes.len()),
            );
            (bytes, true)
        }
        Ok(Err(_)) => {
            dashboard_diagnostic_marker(
                invocation,
                operation,
                name,
                started,
                "read_error",
                "bytes=0",
            );
            (Vec::new(), false)
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            dashboard_diagnostic_marker(invocation, operation, name, started, "timeout", "bytes=0");
            retain_pending_pipe(
                receiver,
                invocation,
                operation,
                name.strip_suffix("_eof").unwrap_or(name),
            );
            (Vec::new(), false)
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            dashboard_diagnostic_marker(
                invocation,
                operation,
                name,
                started,
                "disconnected",
                "bytes=0",
            );
            (Vec::new(), false)
        }
    }
}

fn unsuccessful_diagnostic_status() -> std::process::ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(1 << 8)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(1)
    }
}

fn run_dashboard_with_diagnostics(
    mut command: Command,
    socket_dir: &std::path::Path,
    args: &[&str],
) -> Output {
    let invocation = NEXT_DASHBOARD_DIAGNOSTIC_ID.fetch_add(1, Ordering::SeqCst);
    let operation = dashboard_operation(args);
    let port = dashboard_probe_port(operation, args, socket_dir);
    let sidecars_before =
        socket_dir.join("dashboard.pid").exists() && socket_dir.join("dashboard.config").exists();
    let spawn_started = Instant::now();
    eprintln!(
        "DASHBOARD_DIAG invocation={invocation} operation={operation} stage=launcher_spawn_begin elapsed_ms=0 outcome=started"
    );
    let spawned = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => {
            dashboard_diagnostic_marker(
                invocation,
                operation,
                "launcher_spawn",
                spawn_started,
                "ok",
                "",
            );
            child
        }
        Err(_) => {
            dashboard_diagnostic_marker(
                invocation,
                operation,
                "launcher_spawn",
                spawn_started,
                "error",
                "",
            );
            let sidecars_present = socket_dir.join("dashboard.pid").exists()
                && socket_dir.join("dashboard.config").exists();
            eprintln!(
                "DASHBOARD_DIAG invocation={invocation} operation={operation} stage=dashboard_sidecars elapsed_ms=0 outcome=checked before={} after={} details=content_omitted",
                if sidecars_before { "present" } else { "absent" },
                if sidecars_present { "present" } else { "absent" },
            );
            eprintln!(
                "DASHBOARD_DIAG invocation={invocation} operation={operation} stage=dashboard_worker_listener elapsed_ms=0 outcome=skipped reason=launcher_spawn_failed"
            );
            eprintln!(
                "DASHBOARD_DIAG invocation={invocation} operation={operation} stage=capture_complete elapsed_ms=0 outcome=incomplete"
            );
            DASHBOARD_DIAGNOSTIC_INCOMPLETE.with(|incomplete| incomplete.set(true));
            return Output {
                status: unsuccessful_diagnostic_status(),
                stdout: Vec::new(),
                stderr: Vec::new(),
            };
        }
    };

    let stdout = child.stdout.take().expect("diagnostic stdout pipe");
    let stderr = child.stderr.take().expect("diagnostic stderr pipe");
    let (stdout_sender, stdout_receiver) = mpsc::channel();
    let (stderr_sender, stderr_receiver) = mpsc::channel();
    read_pipe_to_channel(stdout, stdout_sender);
    read_pipe_to_channel(stderr, stderr_sender);

    let launcher_wait_started = Instant::now();
    let deadline = launcher_wait_started + LAUNCHER_EXIT_TIMEOUT;
    let (status, launcher_timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status, false),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                let _ = child.kill();
                let status = child.wait().expect("wait for timed-out dashboard launcher");
                break (status, true);
            }
            Err(_) => {
                let _ = child.kill();
                let status = child
                    .wait()
                    .expect("wait for failed dashboard launcher poll");
                break (status, true);
            }
        }
    };
    let outcome = if launcher_timed_out {
        "timeout"
    } else {
        "exited"
    };
    let exit_status = status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "signal".to_string());
    dashboard_diagnostic_marker(
        invocation,
        operation,
        "launcher_exit",
        launcher_wait_started,
        outcome,
        &format!("exit_status={exit_status}"),
    );

    let (stdout, stdout_complete) =
        collect_diagnostic_pipe(stdout_receiver, invocation, operation, "stdout_eof");
    let (stderr, stderr_complete) =
        collect_diagnostic_pipe(stderr_receiver, invocation, operation, "stderr_eof");

    let sidecars_present =
        socket_dir.join("dashboard.pid").exists() && socket_dir.join("dashboard.config").exists();
    eprintln!(
        "DASHBOARD_DIAG invocation={invocation} operation={operation} stage=dashboard_sidecars elapsed_ms=0 outcome=checked before={} after={} details=content_omitted",
        if sidecars_before { "present" } else { "absent" },
        if sidecars_present { "present" } else { "absent" },
    );
    let should_probe_worker = operation == "stop"
        || (operation == "start"
            && !sidecars_before
            && (status.success() || sidecars_present || launcher_timed_out));
    if let Some(port) = port.filter(|_| should_probe_worker) {
        let worker_probe_started = Instant::now();
        let listening = dashboard_worker_is_listening(port, WORKER_PROBE_TIMEOUT);
        dashboard_diagnostic_marker(
            invocation,
            operation,
            "dashboard_worker_listener",
            worker_probe_started,
            if listening {
                "listening"
            } else {
                "not_listening"
            },
            "address_omitted",
        );
    } else {
        eprintln!(
            "DASHBOARD_DIAG invocation={invocation} operation={operation} stage=dashboard_worker_listener elapsed_ms=0 outcome=skipped reason={}",
            if sidecars_before {
                "preexisting_dashboard"
            } else {
                "no_verified_start"
            }
        );
    }

    let diagnostic_complete = !launcher_timed_out && stdout_complete && stderr_complete;
    eprintln!(
        "DASHBOARD_DIAG invocation={invocation} operation={operation} stage=capture_complete elapsed_ms=0 outcome={}",
        if diagnostic_complete { "complete" } else { "incomplete" }
    );
    if !diagnostic_complete {
        DASHBOARD_DIAGNOSTIC_INCOMPLETE.with(|incomplete| incomplete.set(true));
    }
    let status = if diagnostic_complete || !status.success() {
        status
    } else {
        unsuccessful_diagnostic_status()
    };

    Output {
        status,
        stdout,
        stderr,
    }
}

fn json_output(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout was not JSON: {error}\nstdout:\n{}\nstderr:\n{}",
            dashboard_test_output(&output.stdout),
            dashboard_test_output(&output.stderr)
        )
    })
}

#[test]
fn explicit_dashboard_start_accepts_mcp_style_arguments() {
    let _diagnostic_check = DashboardDiagnosticCheck::new();
    let tmp = TempDir::new().unwrap();
    let _cleanup = DashboardCleanup(&tmp);
    let port = unused_loopback_port();
    let port_arg = port.to_string();

    let started = run_dashboard(
        &tmp,
        &[
            "dashboard",
            "start",
            "--port",
            &port_arg,
            "--allowed-origins",
            "https://dashboard.example.com",
            "--json",
        ],
    );
    assert!(
        started.status.success(),
        "dashboard start failed: {}",
        dashboard_test_output(&started.stderr)
    );
    let payload = json_output(&started);
    assert_eq!(payload["data"]["port"], port);
    let access_urls = payload["data"]["access_urls"].as_array().unwrap();
    assert_eq!(access_urls.len(), 1);
    assert!(access_urls[0]
        .as_str()
        .is_some_and(|url| url.starts_with("https://dashboard.example.com/")));
    assert!(access_urls
        .iter()
        .all(|url| !url.as_str().is_some_and(|url| url.contains("localhost"))));
    let config: Value =
        serde_json::from_slice(&std::fs::read(socket_dir(&tmp).join("dashboard.config")).unwrap())
            .unwrap();
    assert!(config["access_token"]
        .as_str()
        .is_some_and(|token| token.len() == 64));
    #[cfg(unix)]
    assert_eq!(
        std::fs::metadata(socket_dir(&tmp).join("dashboard.config"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn dashboard_listens_on_and_authorizes_ipv6_loopback() {
    let _diagnostic_check = DashboardDiagnosticCheck::new();
    // Some minimal CI/container kernels disable IPv6 entirely. In that case
    // the production server deliberately remains available on IPv4 only.
    if TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).is_err() {
        return;
    }

    let tmp = TempDir::new().unwrap();
    let _cleanup = DashboardCleanup(&tmp);
    let port = unused_loopback_port();
    let port_arg = port.to_string();

    let started = run_dashboard(&tmp, &["dashboard", "start", "--port", &port_arg, "--json"]);
    assert!(started.status.success());

    let response = wait_for_dashboard(
        SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
        &format!("[::1]:{port}"),
        &format!("http://[::1]:{port}"),
    );
    assert!(
        response.starts_with("HTTP/1.1 200 OK"),
        "unexpected IPv6 dashboard response: {}",
        dashboard_test_output(response.as_bytes())
    );
}

#[test]
fn invalid_dashboard_options_fail_without_starting_a_server() {
    let _diagnostic_check = DashboardDiagnosticCheck::new();
    let cases: &[&[&str]] = &[
        &["dashboard", "--bogus", "--json"],
        &[
            "dashboard",
            "start",
            "--allowed-origin",
            "https://dashboard.example.com",
            "--json",
        ],
        &["dashboard", "start", "--port", "nope", "--json"],
        &["dashboard", "start", "--port", "0", "--json"],
        &["dashboard", "start", "--port", "--json"],
        &[
            "dashboard",
            "start",
            "--allowed-origins",
            "https://dashboard.example.com,invalid",
            "--json",
        ],
    ];

    for args in cases {
        let tmp = TempDir::new().unwrap();
        let output = run_dashboard(&tmp, args);
        assert!(
            !output.status.success(),
            "invalid arguments unexpectedly succeeded: {args:?}"
        );
        assert_eq!(json_output(&output)["success"], false);
        assert!(!socket_dir(&tmp).join("dashboard.pid").exists());
        assert!(!socket_dir(&tmp).join("dashboard.config").exists());
    }
}

#[test]
fn running_dashboard_rejects_configuration_changes() {
    let _diagnostic_check = DashboardDiagnosticCheck::new();
    let tmp = TempDir::new().unwrap();
    let port = unused_loopback_port();
    let _dashboard = seed_running_dashboard(
        &tmp,
        port,
        &[
            "https://dashboard.example.com",
            "https://second.example.com",
        ],
    );
    let port_arg = port.to_string();
    let start_args = [
        "dashboard",
        "--port",
        &port_arg,
        "--allowed-origins",
        "https://dashboard.example.com,https://second.example.com",
        "--json",
    ];

    let repeated = run_dashboard(&tmp, &start_args);
    assert!(repeated.status.success());
    assert_eq!(json_output(&repeated)["data"]["already_running"], true);

    let changed = run_dashboard(
        &tmp,
        &[
            "dashboard",
            "start",
            "--port",
            &port_arg,
            "--allowed-origins",
            "https://different.example.com",
            "--json",
        ],
    );
    assert!(
        !changed.status.success(),
        "changed allowlist unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        dashboard_test_output(&changed.stdout),
        dashboard_test_output(&changed.stderr)
    );
    let payload = json_output(&changed);
    assert_eq!(payload["success"], false);
    assert!(payload["error"]
        .as_str()
        .is_some_and(|error| error.contains("dashboard stop")));

    let changed_port = "1";
    let changed = run_dashboard(
        &tmp,
        &[
            "dashboard",
            "start",
            "--port",
            changed_port,
            "--allowed-origins",
            "https://dashboard.example.com,https://second.example.com",
            "--json",
        ],
    );
    assert!(
        !changed.status.success(),
        "changed port unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        dashboard_test_output(&changed.stdout),
        dashboard_test_output(&changed.stderr)
    );
    assert!(json_output(&changed)["error"]
        .as_str()
        .is_some_and(|error| error.contains("dashboard stop")));
}
