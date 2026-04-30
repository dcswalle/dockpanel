use crate::safe_cmd::safe_command;

/// Result of an end-to-end backup drill (extract → scratch container → HTTP probe → teardown).
/// Distinct from `backup_verify::VerificationResult`: a drill *runs* the restored backup,
/// it doesn't just validate the archive.
#[derive(serde::Serialize)]
pub struct DrillResult {
    pub passed: bool,
    pub http_status: Option<i32>,
    pub body_excerpt: Option<String>,
    pub error_message: Option<String>,
    pub duration_ms: u64,
}

fn drill_failure(start: std::time::Instant, msg: impl Into<String>) -> DrillResult {
    DrillResult {
        passed: false,
        http_status: None,
        body_excerpt: None,
        error_message: Some(msg.into()),
        duration_ms: start.elapsed().as_millis() as u64,
    }
}

/// Site drill: extract the backup tar to a scratch dir, mount it read-only into a fresh
/// `nginx:alpine` container with `--network none`, probe via `docker exec wget`, tear everything down.
///
/// Probe success criteria: nginx returns *any* HTTP response (even 403/404 means
/// the container booted and the mount worked). Total HTTP failure (no response,
/// connection refused, exec error) is the failure signal.
pub async fn drill_site_backup(domain: &str, filename: &str) -> Result<DrillResult, String> {
    let start = std::time::Instant::now();

    // Validation mirrors backup_verify::verify_site_backup.
    if filename.is_empty() || filename.contains("..") || filename.contains('/') {
        return Err("Invalid filename".to_string());
    }

    let backup_path = format!("/var/backups/dockpanel/{domain}/{filename}");
    if !std::path::Path::new(&backup_path).exists() {
        return Err("Backup file not found".to_string());
    }

    let drill_id = uuid::Uuid::new_v4().to_string();
    let scratch_dir = format!("/var/lib/dockpanel/drills/{drill_id}");
    let container_name = format!("dockpanel-drill-{}", &drill_id[..8]);

    // Always tear down on exit. Use a guard pattern via an inner async block.
    let result = run_site_drill(&backup_path, &scratch_dir, &container_name, start).await;

    // Cleanup container (best-effort — `--rm` should already handle it but make sure).
    let _ = safe_command("docker")
        .args(["rm", "-f", &container_name])
        .output()
        .await;

    // Cleanup scratch dir (best-effort).
    let _ = std::fs::remove_dir_all(&scratch_dir);

    Ok(result)
}

async fn run_site_drill(
    backup_path: &str,
    scratch_dir: &str,
    container_name: &str,
    start: std::time::Instant,
) -> DrillResult {
    // 1. Create scratch dir.
    if let Err(e) = std::fs::create_dir_all(scratch_dir) {
        return drill_failure(start, format!("scratch dir: {e}"));
    }

    // 2. Extract tar with timeout.
    let extract = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        safe_command("tar")
            .args(["xzf", backup_path, "-C", scratch_dir, "--no-same-owner", "--no-same-permissions"])
            .output(),
    )
    .await;

    let extract_ok = extract
        .map(|r| r.map(|o| o.status.success()).unwrap_or(false))
        .unwrap_or(false);
    if !extract_ok {
        return drill_failure(start, "tar extract failed");
    }

    // 3. Spin nginx:alpine on the scratch dir, read-only mount, no network.
    //    --network none is intentional: a malicious backup can't phone home.
    //    Loopback inside the container still works so wget localhost still does.
    let run = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        safe_command("docker")
            .args([
                "run", "--rm", "-d",
                "--name", container_name,
                "--network", "none",
                "--memory=128m",
                "--cpus=0.5",
                "--read-only",
                "--tmpfs", "/var/cache/nginx",
                "--tmpfs", "/var/run",
                "-v", &format!("{scratch_dir}:/usr/share/nginx/html:ro"),
                "nginx:alpine",
            ])
            .output(),
    )
    .await;

    let started = run
        .map(|r| r.map(|o| o.status.success()).unwrap_or(false))
        .unwrap_or(false);
    if !started {
        return drill_failure(start, "nginx scratch container failed to start");
    }

    // 4. Wait briefly for nginx to bind. Alpine nginx is fast (~200ms on a warm node).
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // 5. Probe via `docker exec wget`. nginx:alpine ships busybox wget.
    //    --server-response prints the status line to stderr; -O - emits body to stdout.
    //    -T 5 caps probe at 5s.
    let probe = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        safe_command("docker")
            .args([
                "exec", container_name,
                "wget", "-q", "-O", "-", "--server-response", "-T", "5",
                "http://localhost/",
            ])
            .output(),
    )
    .await;

    match probe {
        Ok(Ok(out)) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stdout = String::from_utf8_lossy(&out.stdout);
            let http_status = parse_http_status(&stderr);
            let body_excerpt = if stdout.is_empty() { None } else {
                Some(stdout.chars().take(500).collect::<String>())
            };

            // wget exit 0 = 2xx response, exit 8 = server returned non-2xx.
            // Both mean "nginx is alive and serving". Exit 4 = network failure, that's a fail.
            let passed = http_status.is_some();

            DrillResult {
                passed,
                http_status,
                body_excerpt,
                error_message: if passed { None } else {
                    Some(format!("probe got no HTTP response (wget stderr: {})", stderr.chars().take(200).collect::<String>()))
                },
                duration_ms: start.elapsed().as_millis() as u64,
            }
        }
        Ok(Err(e)) => drill_failure(start, format!("docker exec failed: {e}")),
        Err(_) => drill_failure(start, "probe timeout"),
    }
}

/// Parse HTTP status from busybox wget --server-response stderr. Looks for
/// the first line matching `  HTTP/1.x NNN`.
fn parse_http_status(stderr: &str) -> Option<i32> {
    for line in stderr.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("HTTP/") {
            // rest: "1.0 200 OK" or "1.1 404 Not Found"
            let mut parts = rest.split_whitespace();
            let _ver = parts.next()?;
            let code = parts.next()?.parse::<i32>().ok()?;
            return Some(code);
        }
    }
    None
}

// ── Database drills (W1.2.b) ────────────────────────────────────────────────
//
// Distinct from `verify_db_backup` which only confirms the dump *applies*
// (table count from information_schema > 0). A drill goes one step further:
// after restore, it sums actual row counts across user tables. Pass = data
// is queryable post-restore, not just schema applied.

fn is_valid_db_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// DB drill: spin a temp engine container, restore the gzipped dump, then
/// sum row counts across user tables. Pass = restore succeeded AND total
/// rows > 0 (or schema-only databases pass with rows == 0 IFF tables > 0).
pub async fn drill_db_backup(
    db_type: &str,
    db_name: &str,
    filename: &str,
) -> Result<DrillResult, String> {
    let start = std::time::Instant::now();

    if !is_valid_db_name(db_name) {
        return Err("Invalid database name".to_string());
    }
    if !is_valid_db_name(filename) {
        return Err("Invalid filename".to_string());
    }

    let backup_path = format!("/var/backups/dockpanel/databases/{db_name}/{filename}");
    if !std::path::Path::new(&backup_path).exists() {
        return Err("Backup file not found".to_string());
    }

    let drill_id = uuid::Uuid::new_v4().to_string();
    let container_name = format!("dockpanel-drill-db-{}", &drill_id[..8]);
    let test_password = "drill_test_pass_12345";

    let result = match db_type {
        "mysql" | "mariadb" => {
            run_mysql_drill(&backup_path, &container_name, db_name, test_password, start).await
        }
        "postgres" | "postgresql" => {
            run_postgres_drill(&backup_path, &container_name, db_name, test_password, start).await
        }
        _ => drill_failure(start, format!("Unsupported DB type for drill: {db_type}")),
    };

    // Best-effort cleanup of the scratch container.
    let _ = safe_command("docker")
        .args(["rm", "-f", &container_name])
        .output()
        .await;

    Ok(result)
}

async fn run_mysql_drill(
    backup_path: &str,
    container_name: &str,
    db_name: &str,
    password: &str,
    start: std::time::Instant,
) -> DrillResult {
    // 1. Spin temp mariadb. Hardened: --network none (loopback works for the
    //    in-container psql/mariadb client; the engine itself binds 127.0.0.1).
    let start_ok = safe_command("docker")
        .args([
            "run", "-d", "--name", container_name,
            "--network", "none",
            "-e", &format!("MYSQL_DATABASE={db_name}"),
            "-e", &format!("MYSQL_ROOT_PASSWORD={password}"),
            "--memory=256m",
            "--cpus=1.0",
            "mariadb:11",
        ])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !start_ok {
        return drill_failure(start, "mariadb scratch container failed to start");
    }

    // 2. Wait for ready (up to 40s).
    let mut ready = false;
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let check = safe_command("docker")
            .args([
                "exec", "-e", &format!("MYSQL_PWD={password}"),
                container_name, "mariadb", "-u", "root", "-e", "SELECT 1",
            ])
            .output()
            .await;
        if check.map(|o| o.status.success()).unwrap_or(false) {
            ready = true;
            break;
        }
    }
    if !ready {
        return drill_failure(start, "mariadb container not ready within 40s");
    }

    // 3. Restore via zcat → docker exec mariadb (direct fd pipe, no shell).
    let zcat_child = safe_command("zcat")
        .arg(backup_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();

    let restore_ok = match zcat_child {
        Ok(mut zcat) => match zcat.stdout.take() {
            Some(stdout) => {
                let r = tokio::time::timeout(
                    std::time::Duration::from_secs(180),
                    safe_command("docker")
                        .args([
                            "exec", "-i",
                            "-e", &format!("MYSQL_PWD={password}"),
                            container_name, "mariadb", "-u", "root", db_name,
                        ])
                        .stdin(stdout.into_owned_fd().unwrap())
                        .output(),
                )
                .await;
                r.map(|x| x.map(|o| o.status.success()).unwrap_or(false)).unwrap_or(false)
            }
            None => false,
        },
        Err(_) => false,
    };
    if !restore_ok {
        return drill_failure(start, "mariadb restore failed");
    }

    // 4. Smoke query: count user tables AND sum rows. Schema-only dumps pass
    //    with rows == 0 as long as tables > 0 (legitimate edge case).
    let table_count = run_mysql_scalar(
        container_name, password, db_name,
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema=DATABASE()",
    ).await;

    if table_count == 0 {
        return drill_failure(start, "no tables present after restore");
    }

    // information_schema.tables.table_rows is approximate for InnoDB but good
    // enough for drill semantics ("is there *any* data?"). Cheaper than
    // SELECT COUNT(*) per table.
    let row_total = run_mysql_scalar(
        container_name, password, db_name,
        "SELECT IFNULL(SUM(table_rows),0) FROM information_schema.tables WHERE table_schema=DATABASE()",
    ).await;

    DrillResult {
        passed: true,
        http_status: None,
        body_excerpt: Some(format!("{table_count} tables, ~{row_total} rows restored")),
        error_message: None,
        duration_ms: start.elapsed().as_millis() as u64,
    }
}

async fn run_mysql_scalar(container_name: &str, password: &str, db_name: &str, sql: &str) -> i64 {
    safe_command("docker")
        .args([
            "exec", "-e", &format!("MYSQL_PWD={password}"),
            container_name, "mariadb", "-u", "root", db_name,
            "-e", sql, "--batch", "--skip-column-names",
        ])
        .output()
        .await
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<i64>().unwrap_or(0))
        .unwrap_or(0)
}

async fn run_postgres_drill(
    backup_path: &str,
    container_name: &str,
    db_name: &str,
    password: &str,
    start: std::time::Instant,
) -> DrillResult {
    // 1. Spin temp postgres.
    let start_ok = safe_command("docker")
        .args([
            "run", "-d", "--name", container_name,
            "--network", "none",
            "-e", &format!("POSTGRES_DB={db_name}"),
            "-e", "POSTGRES_USER=drill",
            "-e", &format!("POSTGRES_PASSWORD={password}"),
            "--memory=256m",
            "--cpus=1.0",
            "postgres:16-alpine",
        ])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !start_ok {
        return drill_failure(start, "postgres scratch container failed to start");
    }

    // 2. Wait for ready.
    let mut ready = false;
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let check = safe_command("docker")
            .args([
                "exec", "-e", &format!("PGPASSWORD={password}"),
                container_name, "psql", "-U", "drill", "-d", db_name, "-c", "SELECT 1",
            ])
            .output()
            .await;
        if check.map(|o| o.status.success()).unwrap_or(false) {
            ready = true;
            break;
        }
    }
    if !ready {
        return drill_failure(start, "postgres container not ready within 30s");
    }

    // 3. Restore via zcat → psql.
    let zcat_child = safe_command("zcat")
        .arg(backup_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();

    let restore_ok = match zcat_child {
        Ok(mut zcat) => match zcat.stdout.take() {
            Some(stdout) => {
                let r = tokio::time::timeout(
                    std::time::Duration::from_secs(180),
                    safe_command("docker")
                        .args([
                            "exec", "-i",
                            "-e", &format!("PGPASSWORD={password}"),
                            container_name,
                            "psql", "-U", "drill", "-d", db_name, "--quiet",
                            "-v", "ON_ERROR_STOP=1",
                        ])
                        .stdin(stdout.into_owned_fd().unwrap())
                        .output(),
                )
                .await;
                r.map(|x| x.map(|o| o.status.success()).unwrap_or(false)).unwrap_or(false)
            }
            None => false,
        },
        Err(_) => false,
    };
    if !restore_ok {
        return drill_failure(start, "postgres restore failed");
    }

    // 4. Table count + row total via pg_class.reltuples (planner stats; cheap).
    let table_count = run_psql_scalar(
        container_name, password, db_name,
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema='public'",
    ).await;
    if table_count == 0 {
        return drill_failure(start, "no tables present in public schema after restore");
    }

    // ANALYZE to populate reltuples (fresh restore has stats=0). Bounded to 30s
    // so a giant dump doesn't hold the drill open.
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        safe_command("docker")
            .args([
                "exec", "-e", &format!("PGPASSWORD={password}"),
                container_name, "psql", "-U", "drill", "-d", db_name,
                "-c", "ANALYZE",
            ])
            .output(),
    )
    .await;

    let row_total = run_psql_scalar(
        container_name, password, db_name,
        "SELECT COALESCE(SUM(reltuples)::bigint, 0) FROM pg_class WHERE relkind='r' AND relnamespace=(SELECT oid FROM pg_namespace WHERE nspname='public')",
    ).await;

    DrillResult {
        passed: true,
        http_status: None,
        body_excerpt: Some(format!("{table_count} tables, ~{row_total} rows restored")),
        error_message: None,
        duration_ms: start.elapsed().as_millis() as u64,
    }
}

async fn run_psql_scalar(container_name: &str, password: &str, db_name: &str, sql: &str) -> i64 {
    safe_command("docker")
        .args([
            "exec", "-e", &format!("PGPASSWORD={password}"),
            container_name, "psql", "-U", "drill", "-d", db_name,
            "-t", "-A", "-c", sql,
        ])
        .output()
        .await
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<i64>().unwrap_or(0))
        .unwrap_or(0)
}
