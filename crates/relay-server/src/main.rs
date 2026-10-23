use clap::Parser;
use relay_server::{network_ingress_router, router, AppState, RtcConfig, ServerConfig};
use serde_json::{json, Value};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
};
#[cfg(windows)]
mod data_privacy;

#[derive(Parser)]
#[command(
    about = "Relay community server. Localhost by default; administrator initialization required before LAN/public hosting."
)]
struct Options {
    #[arg(long, default_value = "127.0.0.1")]
    bind: IpAddr,
    /// UDP voice interface; defaults to the HTTP --bind address.
    #[arg(long)]
    udp_bind: Option<IpAddr>,
    /// Additional HTTP interface on an installed Tailscale IPv4; keeps local control loopback.
    #[arg(
        long,
        value_parser = parse_tailscale_bind,
        conflicts_with_all = [
            "backup", "raw_backup", "restore", "init_admin", "recover_admin",
            "migrate_only", "status", "stop", "public_hosting_status"
        ]
    )]
    tailscale_bind: Option<Ipv4Addr>,
    /// Isolated loopback HTTP port for the authenticated embedded-network helper.
    #[arg(
        long,
        conflicts_with_all = [
            "tailscale_bind", "backup", "raw_backup", "restore", "init_admin",
            "recover_admin", "migrate_only", "status", "stop", "public_hosting_status"
        ]
    )]
    network_peer_port: Option<u16>,
    #[arg(long, default_value_t = 4580)]
    port: u16,
    #[arg(long, default_value_t = 4581)]
    udp_port: u16,
    #[arg(long)]
    data: Option<PathBuf>,
    /// Account-wide friends, private messages and calls. Use a separate new --data directory.
    #[arg(long)]
    people: bool,
    #[arg(long, default_value = "Relay House")]
    name: String,
    #[arg(long,default_value_t=10*1024*1024*1024)]
    storage_limit: u64,
    #[arg(long,default_value_t=1024*1024*1024)]
    user_storage_limit: u64,
    #[arg(long)]
    trusted_proxy: Vec<IpAddr>,
    /// Additional exact public DNS names (repeatable). IP literals work without this.
    #[arg(long)]
    allowed_host: Vec<String>,
    #[arg(long)]
    backup: Option<PathBuf>,
    /// Snapshot an existing database without applying schema migrations.
    #[arg(long)]
    raw_backup: Option<PathBuf>,
    #[arg(long)]
    restore: Option<PathBuf>,
    #[arg(long)]
    init_admin: Option<String>,
    #[arg(long)]
    recover_admin: Option<String>,
    #[arg(long)]
    migrate_only: bool,
    #[arg(long)]
    status: bool,
    #[arg(long)]
    stop: bool,
    /// Check an existing community owner without opening a server or changing data.
    #[arg(
        long,
        conflicts_with_all = [
            "backup", "raw_backup", "restore", "init_admin", "recover_admin",
            "migrate_only", "status", "stop"
        ]
    )]
    public_hosting_status: bool,
    /// Inspect public installation state without opening server data or a runtime.
    #[arg(long)]
    installation_status: bool,
    /// Wait for the exact installer process to exit, then inspect installation state.
    #[arg(long, value_name = "PID-LOW-HIGH", require_equals = true)]
    wait_for_installer: Option<String>,
}
fn parse_tailscale_bind(value: &str) -> Result<Ipv4Addr, String> {
    let address: Ipv4Addr = value
        .parse()
        .map_err(|_| "Tailscale hosting requires an installed IPv4 address in 100.64.0.0/10")?;
    let octets = address.octets();
    if octets[0] != 100 || !(64..=127).contains(&octets[1]) {
        return Err("Tailscale hosting requires an installed IPv4 address in 100.64.0.0/10".into());
    }
    Ok(address)
}
fn resolved_udp_bind(options: &Options) -> Result<IpAddr, &'static str> {
    if let Some(peer_port) = options.network_peer_port {
        if options.tailscale_bind.is_some()
            || !options.bind.is_loopback()
            || !options.bind.is_ipv4()
        {
            return Err("Embedded-network hosting requires an IPv4 loopback --bind and cannot be combined with --tailscale-bind");
        }
        if options
            .udp_bind
            .is_some_and(|address| !address.is_loopback() || !address.is_ipv4())
        {
            return Err("Embedded-network hosting requires a loopback UDP interface; public UDP binding is not permitted in this mode");
        }
        if peer_port != 0 && peer_port == options.port {
            return Err("Embedded-network peer ingress must use a different port from the private owner listener");
        }
    }
    if options.tailscale_bind.is_some() {
        if !options.bind.is_loopback() || !options.bind.is_ipv4() {
            return Err("Tailscale hosting requires an IPv4 loopback --bind address, such as 127.0.0.1, for local app and private management access");
        }
        let wildcard = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        if options.udp_bind.is_some_and(|address| address != wildcard) {
            return Err("Tailscale hosting requires --udp-bind 0.0.0.0, or omit --udp-bind to select it automatically");
        }
        return Ok(wildcard);
    }
    Ok(options.udp_bind.unwrap_or(options.bind))
}
fn requires_admin_initialization(http_bind: IpAddr, udp_bind: IpAddr) -> bool {
    !http_bind.is_loopback() || !udp_bind.is_loopback()
}
fn managed_control_address(http_bind: IpAddr, port: u16) -> SocketAddr {
    let control_ip = if http_bind.is_unspecified() {
        if http_bind.is_ipv4() {
            IpAddr::from([127, 0, 0, 1])
        } else {
            IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
        }
    } else {
        http_bind
    };
    SocketAddr::new(control_ip, port)
}
fn default_data() -> PathBuf {
    if let Some(data) = std::env::var_os("RELAY_SERVER_DATA_DIR") {
        return PathBuf::from(data);
    }
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|p| p.join("Relay").join("server-data"))
        .unwrap_or_else(|| PathBuf::from("relay-data"))
}
fn public_hosting_status(data: &Path) -> Value {
    let failure = |code: &str, message: &str| {
        json!({
            "ready": false,
            "owner_initialized": false,
            "server_id": null,
            "error_code": code,
            "message": message
        })
    };
    let database = data.join("relay.sqlite3");
    match std::fs::metadata(&database) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => {
            return failure(
                "community_unreadable",
                "Community database is not a file. Choose the existing Relay community folder.",
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return failure(
                "community_missing",
                "Start a local Relay server and create its owner account before enabling public hosting.",
            );
        }
        Err(_) => {
            return failure(
                "community_unreadable",
                "Cannot read the existing community database. Check access to its data folder.",
            );
        }
    }
    // Even SQLITE_OPEN_READ_ONLY can create WAL/shared-memory sidecars. An
    // immutable snapshot avoids all writes, but must not ignore pending WAL
    // transactions or a hot rollback journal. Fail closed until local shutdown
    // has checkpointed/recovered the existing database.
    for suffix in ["-wal", "-journal"] {
        let sidecar = data.join(format!("relay.sqlite3{suffix}"));
        match std::fs::metadata(sidecar) {
            Ok(metadata) if metadata.len() > 0 => {
                return failure(
                    "community_busy",
                    "Stop the local Relay host normally before enabling public hosting, so its database can be checked without changing it.",
                );
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return failure(
                    "community_unreadable",
                    "Cannot verify the existing database journal. Check access to the community data folder.",
                );
            }
        }
    }
    let absolute = if database.is_absolute() {
        database
    } else {
        match std::env::current_dir() {
            Ok(current) => current.join(database),
            Err(_) => {
                return failure(
                    "community_unreadable",
                    "Cannot resolve the community data folder. Choose an absolute data folder path.",
                );
            }
        }
    };
    let mut uri = match reqwest::Url::from_file_path(absolute) {
        Ok(uri) => uri,
        Err(_) => {
            return failure(
                "community_unreadable",
                "Cannot resolve the community database path. Choose an absolute local data folder.",
            );
        }
    };
    uri.set_query(Some("mode=ro&immutable=1"));
    let checked = (|| -> rusqlite::Result<(String, bool)> {
        let db = rusqlite::Connection::open_with_flags(
            uri.as_str(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )?;
        let schema: u32 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if schema != relay_server::SCHEMA_VERSION {
            return Err(rusqlite::Error::InvalidParameterName(
                "schema_update_required".into(),
            ));
        }
        let server_id: String = db.query_row(
            "SELECT value FROM metadata WHERE key='server_id'",
            [],
            |row| row.get(0),
        )?;
        // Matches the active administrator invariant maintained by community.rs
        // and the role/membership written by initialize_admin.
        let owner_initialized: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM users u JOIN membership m ON m.user_id=u.id WHERE u.role='admin' AND m.status='active')",
            [],
            |row| row.get(0),
        )?;
        Ok((server_id, owner_initialized))
    })();
    match checked {
        Ok((server_id, true)) if uuid::Uuid::parse_str(&server_id).is_ok() => json!({
            "ready": true,
            "owner_initialized": true,
            "server_id": server_id,
            "error_code": null,
            "message": "Existing community owner verified."
        }),
        Ok((_, false)) => failure(
            "owner_missing",
            "Create or recover an active owner account on the local Relay server before enabling public hosting.",
        ),
        Err(rusqlite::Error::InvalidParameterName(code)) if code == "schema_update_required" => {
            failure(
                "schema_update_required",
                "Start and stop this community locally with the matching Relay version before enabling public hosting. No migrations were performed by this check.",
            )
        }
        _ => failure(
            "community_invalid",
            "Cannot verify this community's identity and active owner. Open it locally or restore a verified backup before enabling public hosting.",
        ),
    }
}
fn read_managed(data: &Path) -> Result<Value, Box<dyn std::error::Error>> {
    Ok(serde_json::from_slice(&std::fs::read(
        data.join("server-runtime.json"),
    )?)?)
}
async fn control(data: &Path, stop: bool) -> Result<(), Box<dyn std::error::Error>> {
    let info = read_managed(data)?;
    let address = info["address"].as_str().ok_or("Invalid managed address")?;
    let url = reqwest::Url::parse(address)?;
    if !url.host_str().is_some_and(|host| {
        host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
    }) {
        return Err("Managed control address must be loopback".into());
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let health: Value = client
        .get(format!("{address}/api/health"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if health["instance_id"] != info["instance_id"]
        || health["server_id"] != info["server_id"]
        || health["pid"] != info["pid"]
    {
        return Err("Managed record belongs to a different server instance".into());
    }
    if stop {
        client
            .post(format!("{address}/api/local/shutdown"))
            .bearer_auth(
                info["shutdown_token"]
                    .as_str()
                    .ok_or("Missing management token")?,
            )
            .json(&json!({"instance_id":info["instance_id"],"confirm_server_id":info["server_id"]}))
            .send()
            .await?
            .error_for_status()?;
        println!("Graceful shutdown requested");
    } else {
        println!("{}", serde_json::to_string_pretty(&health)?);
    }
    Ok(())
}
fn private_record(path: &Path, value: &Value) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    let temporary = path.with_extension("json.new");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.sync_all()?;
    drop(file);
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    std::fs::rename(temporary, path)?;
    Ok(())
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let installation = relay_protocol::installation::inspect_startup(&args);
    if args.iter().any(|arg| arg == "--installation-status") {
        println!("{}", installation.diagnostic());
        std::process::exit(if installation.is_ready() { 0 } else { 2 });
    }
    if !installation.is_ready() {
        eprintln!("Installation needs repair. Run the Relay installer again.");
        std::process::exit(2);
    }
    let options = Options::parse();
    if options.public_hosting_status {
        let data = options
            .data
            .as_deref()
            .map(Path::to_path_buf)
            .unwrap_or_else(default_data);
        let status = public_hosting_status(&data);
        println!("{status}");
        std::process::exit(if status["ready"] == true { 0 } else { 2 });
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run(options))
}

async fn run(options: Options) -> Result<(), Box<dyn std::error::Error>> {
    let udp_bind = resolved_udp_bind(&options)?;
    let data = options.data.unwrap_or_else(default_data);
    if options.tailscale_bind.is_some() || options.network_peer_port.is_some() {
        let status = public_hosting_status(&data);
        if status["ready"] != true {
            return Err(status["message"]
                .as_str()
                .unwrap_or("Create the owner locally and stop its local host before starting Tailscale hosting")
                .to_owned()
                .into());
        }
    }
    if options.status || options.stop {
        return control(&data, options.stop).await;
    }
    let modes = usize::from(options.backup.is_some())
        + usize::from(options.raw_backup.is_some())
        + usize::from(options.restore.is_some())
        + usize::from(options.init_admin.is_some())
        + usize::from(options.recover_admin.is_some())
        + usize::from(options.migrate_only);
    if modes > 1 {
        return Err("Choose only one maintenance command".into());
    }
    #[cfg(windows)]
    data_privacy::protect_data_directory(&data)?;
    #[cfg(not(windows))]
    std::fs::create_dir_all(&data)?;
    let data = std::fs::canonicalize(data)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(data.join("server.lock"))?;
    lock.try_lock().map_err(|_| {
        "This data directory is in use; stop its server before maintenance or a second launch"
    })?;
    let mut config = ServerConfig {
        people: options.people,
        account_provider: relay_server::AccountProviderConfig::configured()?,
        storage_limit_bytes: options.storage_limit,
        user_storage_limit_bytes: options.user_storage_limit,
        trusted_proxy_ips: options.trusted_proxy,
        allowed_hosts: options
            .allowed_host
            .into_iter()
            .chain(
                std::env::var("RELAY_ALLOWED_HOSTS")
                    .ok()
                    .into_iter()
                    .flat_map(|v| {
                        v.split(',')
                            .map(str::trim)
                            .filter(|v| !v.is_empty())
                            .map(str::to_owned)
                            .collect::<Vec<_>>()
                    }),
            )
            .collect(),
        ..Default::default()
    };
    let secret = uuid::Uuid::new_v4().to_string() + &uuid::Uuid::new_v4().to_string();
    config.local_control_token = Some(secret.clone());
    if let Some(destination) = options.raw_backup {
        let db = rusqlite::Connection::open_with_flags(
            data.join("relay.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        relay_server::backup_database(&db, &destination)?;
        println!("Raw snapshot verified; source schema unchanged");
        return Ok(());
    }
    if modes > 0 {
        let state = AppState::open_with_config(&data, &options.name, 0, config)?;
        if let Some(destination) = options.backup {
            state.backup_to(&destination)?;
            println!("Backup verified: {}", destination.display());
        }
        if let Some(source) = options.restore {
            state.restore_from(&source)?;
            println!("Restore verified; sessions revoked; recovery backup preserved");
        }
        if let Some(username) = options.init_admin {
            let password = std::env::var("RELAY_ADMIN_PASSWORD")
                .map_err(|_| "Set RELAY_ADMIN_PASSWORD before initializing")?;
            state.initialize_admin(&username, &password)?;
            println!("Administrator initialized; server {}", state.server_id()?);
        }
        if let Some(username) = options.recover_admin {
            let password = std::env::var("RELAY_ADMIN_PASSWORD")
                .map_err(|_| "Set RELAY_ADMIN_PASSWORD before recovery")?;
            state.recover_password(&username, &password)?;
            println!("Administrator recovered; previous sessions revoked");
        }
        if options.migrate_only {
            println!("Schema current; server {}", state.server_id()?);
        }
        return Ok(());
    }
    config.rtc = RtcConfig::from_env()?;
    let udp = tokio::net::UdpSocket::bind(SocketAddr::new(udp_bind, options.udp_port)).await?;
    mark_voice_priority(&udp);
    let udp_address = udp.local_addr()?;
    let state = AppState::open_with_config(&data, &options.name, udp_address.port(), config)?;
    if !options.people
        && requires_admin_initialization(options.bind, udp_bind)
        && !state.has_users()?
    {
        return Err("Initialize the administrator locally with --init-admin before binding to LAN/public interfaces".into());
    }
    let http = tokio::net::TcpListener::bind(SocketAddr::new(options.bind, options.port)).await?;
    let http_address = http.local_addr()?;
    // Bind both listeners before serving either; failure cannot leave a partial
    // hosting mode or allow a peer to claim the first local owner account.
    let tailscale_http = if let Some(peer_bind) = options.tailscale_bind {
        Some(
            tokio::net::TcpListener::bind(SocketAddr::new(
                IpAddr::V4(peer_bind),
                http_address.port(),
            ))
            .await?,
        )
    } else {
        None
    };
    let network_http = if let Some(port) = options.network_peer_port {
        Some(
            tokio::net::TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
                .await?,
        )
    } else {
        None
    };
    let network_peer_port = network_http
        .as_ref()
        .map(|listener| listener.local_addr().map(|address| address.port()))
        .transpose()?;
    let network_ingress_token = network_peer_port
        .map(|_| uuid::Uuid::new_v4().to_string() + &uuid::Uuid::new_v4().to_string());
    let address = format!(
        "http://{}",
        managed_control_address(options.bind, http_address.port())
    );
    private_record(
        &data.join("server-runtime.json"),
        &json!({"instance_id":state.instance_id(),"server_id":state.server_id()?,"pid":std::process::id(),"started_at":chrono::Utc::now().to_rfc3339(),"exe":std::env::current_exe()?,"data":data,"address":address,"shutdown_token":secret,"network_peer_port":network_peer_port,"network_ingress_token":network_ingress_token}),
    )?;
    private_record(
        &data.join("managed-instance.json"),
        &json!({"instance_id":state.instance_id(),"server_id":state.server_id()?,"pid":std::process::id(),"exe":std::env::current_exe()?,"data":data,"url":address}),
    )?;
    let udp_state = state.clone();
    let tailscale_only = options.tailscale_bind.is_some();
    let udp_task = tokio::spawn(async move {
        if tailscale_only {
            relay_server::run_udp_tailscale(udp, udp_state).await
        } else {
            relay_server::run_udp(udp, udp_state).await
        }
    });
    let cleanup_state = state.clone();
    let cleanup = tokio::spawn(async move { relay_server::cleanup_loop(cleanup_state).await });
    let discord_state = state.clone();
    let discord_token = std::env::var("RELAY_DISCORD_BOT_TOKEN").ok();
    let discord = tokio::spawn(async move {
        relay_server::run_discord_bridge(discord_state, discord_token).await
    });
    println!("Relay HTTP listening on {http_address}");
    if let Some(listener) = &tailscale_http {
        println!(
            "Relay Tailscale HTTP listening on {}",
            listener.local_addr()?
        );
    }
    println!("Relay UDP listening on {udp_address}");
    let shutdown_state = state.clone();
    let shutdown = tokio::spawn(async move {
        tokio::select! {_=tokio::signal::ctrl_c()=>{},_=shutdown_state.shutdown_requested()=>{}}
        shutdown_state.request_shutdown();
    });
    let serving = match (tailscale_http, network_http, network_ingress_token) {
        (Some(peer_listener), None, None) => tokio::try_join!(
            serve_http(http, state.clone()),
            serve_http(peer_listener, state.clone())
        )
        .map(|_| ()),
        (None, Some(peer_listener), Some(token)) => tokio::try_join!(
            serve_http(http, state.clone()),
            serve_network_http(peer_listener, state.clone(), token)
        )
        .map(|_| ()),
        (None, None, None) => serve_http(http, state.clone()).await,
        _ => unreachable!("binding validation permits only one peer transport"),
    };
    state.request_shutdown();
    shutdown.abort();
    udp_task.abort();
    cleanup.abort();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(7), discord).await;
    let _ = std::fs::remove_file(data.join("server-runtime.json"));
    drop(lock);
    serving?;
    Ok(())
}

async fn serve_http(listener: tokio::net::TcpListener, state: AppState) -> std::io::Result<()> {
    let shutdown = state.clone();
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move { shutdown.shutdown_requested().await })
    .await
}

async fn serve_network_http(
    listener: tokio::net::TcpListener,
    state: AppState,
    token: String,
) -> std::io::Result<()> {
    let shutdown = state.clone();
    axum::serve(
        listener,
        network_ingress_router(state, token).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move { shutdown.shutdown_requested().await })
    .await
}

fn mark_voice_priority(socket: &tokio::net::UdpSocket) {
    #[cfg(unix)]
    {
        let socket = socket2::SockRef::from(socket);
        let result = if socket.local_addr().is_ok_and(|a| a.is_ipv6()) {
            socket.set_tclass_v6(0xB8)
        } else {
            socket.set_tos_v4(0xB8)
        };
        if let Err(error) = result {
            eprintln!("Voice packet priority unavailable: {error}");
        }
    }
    #[cfg(not(unix))]
    let _ = socket;
}
#[cfg(test)]
mod tests {
    use super::*;

    fn owner_status_fixture(role: &str, membership: &str, schema: u32) -> tempfile::TempDir {
        let data = tempfile::Builder::new()
            .prefix("relay owner status ")
            .tempdir()
            .unwrap();
        let db = rusqlite::Connection::open(data.path().join("relay.sqlite3")).unwrap();
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
             CREATE TABLE users(id TEXT PRIMARY KEY,username TEXT NOT NULL UNIQUE COLLATE NOCASE,password_hash TEXT NOT NULL,role TEXT NOT NULL);
             CREATE TABLE membership(user_id TEXT PRIMARY KEY REFERENCES users(id),status TEXT NOT NULL DEFAULT 'active');
             INSERT INTO metadata VALUES('server_id','13a1ff85-9231-4d45-9b8c-a00000000001');",
        )
        .unwrap();
        db.pragma_update(None, "user_version", schema).unwrap();
        db.execute(
            "INSERT INTO users VALUES('fixture-owner','FixtureOwner','fixture-not-a-real-password-hash',?1)",
            [role],
        )
        .unwrap();
        db.execute(
            "INSERT INTO membership VALUES('fixture-owner',?1)",
            [membership],
        )
        .unwrap();
        drop(db);
        data
    }

    fn fixture_files(data: &Path) -> Vec<(std::ffi::OsString, Vec<u8>)> {
        let mut files: Vec<_> = std::fs::read_dir(data)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (entry.file_name(), std::fs::read(entry.path()).unwrap())
            })
            .collect();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        files
    }

    #[test]
    fn public_owner_check_is_read_only_even_for_checkpointed_wal_database() {
        let data = owner_status_fixture("admin", "active", relay_server::SCHEMA_VERSION);
        let before = fixture_files(data.path());
        assert_eq!(before.len(), 1);
        let status = public_hosting_status(data.path());
        assert_eq!(status["ready"], true);
        assert_eq!(status["owner_initialized"], true);
        assert_eq!(status["server_id"], "13a1ff85-9231-4d45-9b8c-a00000000001");
        assert_eq!(fixture_files(data.path()), before);
    }

    #[test]
    fn public_owner_check_requires_an_active_administrator_not_just_any_user() {
        for (role, membership) in [("member", "active"), ("admin", "banned")] {
            let data = owner_status_fixture(role, membership, relay_server::SCHEMA_VERSION);
            let before = fixture_files(data.path());
            let status = public_hosting_status(data.path());
            assert_eq!(status["ready"], false);
            assert_eq!(status["owner_initialized"], false);
            assert_eq!(status["error_code"], "owner_missing");
            assert_eq!(fixture_files(data.path()), before);
        }
    }

    #[test]
    fn public_owner_check_never_creates_a_missing_community() {
        let fixture = tempfile::tempdir().unwrap();
        let missing = fixture.path().join("community-does-not-exist");
        let status = public_hosting_status(&missing);
        assert_eq!(status["ready"], false);
        assert_eq!(status["error_code"], "community_missing");
        assert!(!missing.exists());
        assert_eq!(std::fs::read_dir(fixture.path()).unwrap().count(), 0);
    }

    #[test]
    fn public_owner_check_does_not_migrate_or_recover_journals() {
        let older = owner_status_fixture("admin", "active", relay_server::SCHEMA_VERSION - 1);
        let before = fixture_files(older.path());
        let status = public_hosting_status(older.path());
        assert_eq!(status["error_code"], "schema_update_required");
        assert_eq!(fixture_files(older.path()), before);

        for suffix in ["-wal", "-journal"] {
            let data = owner_status_fixture("admin", "active", relay_server::SCHEMA_VERSION);
            std::fs::write(
                data.path().join(format!("relay.sqlite3{suffix}")),
                b"pending fixture transaction",
            )
            .unwrap();
            let before = fixture_files(data.path());
            let status = public_hosting_status(data.path());
            assert_eq!(status["ready"], false);
            assert_eq!(status["error_code"], "community_busy");
            assert_eq!(fixture_files(data.path()), before);
        }
    }

    #[test]
    fn public_owner_check_cannot_be_combined_with_mutating_maintenance() {
        assert!(
            Options::try_parse_from(["relay-server", "--public-hosting-status"])
                .unwrap()
                .public_hosting_status
        );
        for mode in ["--migrate-only", "--stop"] {
            assert!(
                Options::try_parse_from(["relay-server", "--public-hosting-status", mode]).is_err()
            );
        }
        assert!(Options::try_parse_from([
            "relay-server",
            "--public-hosting-status",
            "--init-admin",
            "FixtureOwner"
        ])
        .is_err());
    }

    #[test]
    fn tailscale_bind_accepts_only_the_shared_ipv4_range() {
        for address in ["100.64.0.1", "100.127.255.254"] {
            let options =
                Options::try_parse_from(["relay-server", "--tailscale-bind", address]).unwrap();
            assert_eq!(options.tailscale_bind, Some(address.parse().unwrap()));
        }
        for address in [
            "100.63.255.255",
            "100.128.0.0",
            "127.0.0.1",
            "192.168.1.2",
            "8.8.8.8",
            "::1",
            "100.64.0.1:4580",
        ] {
            assert!(
                Options::try_parse_from(["relay-server", "--tailscale-bind", address]).is_err(),
                "unexpected Tailscale address accepted: {address}"
            );
        }
    }

    #[test]
    fn embedded_network_ingress_requires_distinct_loopback_listeners() {
        let defaults =
            Options::try_parse_from(["relay-server", "--network-peer-port", "4582"]).unwrap();
        assert_eq!(defaults.network_peer_port, Some(4582));
        assert_eq!(
            resolved_udp_bind(&defaults).unwrap(),
            "127.0.0.1".parse::<IpAddr>().unwrap()
        );
        for (flag, address) in [
            ("--bind", "0.0.0.0"),
            ("--bind", "100.64.0.1"),
            ("--bind", "::1"),
            ("--udp-bind", "0.0.0.0"),
            ("--udp-bind", "100.64.0.1"),
            ("--network-peer-port", "4580"),
        ] {
            let arguments = if flag == "--network-peer-port" {
                vec!["relay-server", flag, address]
            } else {
                vec!["relay-server", "--network-peer-port", "4582", flag, address]
            };
            let options = Options::try_parse_from(arguments).unwrap();
            assert!(resolved_udp_bind(&options).is_err());
        }
        assert!(Options::try_parse_from([
            "relay-server",
            "--network-peer-port",
            "4582",
            "--tailscale-bind",
            "100.64.0.1"
        ])
        .is_err());
    }

    #[tokio::test]
    async fn embedded_network_ingress_requires_initialized_owner_before_any_side_effects() {
        let fixture = tempfile::tempdir().unwrap();
        let missing = fixture.path().join("missing-network-community");
        let options = Options::try_parse_from([
            "relay-server",
            "--network-peer-port",
            "0",
            "--data",
            missing.to_str().unwrap(),
        ])
        .unwrap();
        let error = run(options).await.unwrap_err().to_string();
        assert!(error.contains("create its owner account"));
        assert!(!missing.exists());
        assert_eq!(std::fs::read_dir(fixture.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn isolated_network_listener_never_inherits_owner_privileges_from_loopback_proxy() {
        const CONTROL: &str = "fixture-owner-process-control-proof";
        const INGRESS: &str = "fixture-network-ingress-capability-distinct-from-control";
        let data = tempfile::tempdir().unwrap();
        let state = AppState::open_with_config(
            data.path(),
            "embedded network fixture",
            0,
            ServerConfig {
                local_control_token: Some(CONTROL.into()),
                ..Default::default()
            },
        )
        .unwrap();
        let owner = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let owner_url = format!("http://{}", owner.local_addr().unwrap());
        let peer_url = format!("http://{}", peer.local_addr().unwrap());
        let owner_task = tokio::spawn(serve_http(owner, state.clone()));
        let peer_task = tokio::spawn(serve_network_http(peer, state.clone(), INGRESS.into()));
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        assert_eq!(
            client
                .get(format!("{peer_url}/api/health"))
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
        let registration =
            json!({"username":"FixtureOwner","password":"fixture-owner-password-123","invite":""});
        // The transactional registration guard also protects accidental helper
        // startup on an empty server, independently of the CLI owner preflight.
        assert_eq!(
            client
                .post(format!("{peer_url}/api/register"))
                .header("x-relay-network-ingress", INGRESS)
                .header("x-relay-network-peer", "100.64.0.7")
                .json(&registration)
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::FORBIDDEN
        );
        assert!(!state.has_users().unwrap());
        let session: relay_protocol::Session = client
            .post(format!("{owner_url}/api/register"))
            .json(&registration)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(session.user.role, "admin");
        for (path, bearer) in [
            ("/api/local/discord-credentials", session.token.as_str()),
            ("/api/local/shutdown", CONTROL),
        ] {
            let request = if path.ends_with("shutdown") {
                client.post(format!("{peer_url}{path}")).json(&json!({"instance_id":state.instance_id(),"confirm_server_id":state.server_id().unwrap()}))
            } else {
                client.get(format!("{peer_url}{path}"))
            };
            assert_eq!(
                request
                    .header("x-relay-network-ingress", INGRESS)
                    .header("x-relay-network-peer", "100.64.0.7")
                    .bearer_auth(bearer)
                    .header("x-relay-local-control", CONTROL)
                    .header("x-relay-instance-id", state.instance_id())
                    .header("x-relay-server-id", state.server_id().unwrap())
                    .send()
                    .await
                    .unwrap()
                    .status(),
                reqwest::StatusCode::FORBIDDEN
            );
        }
        assert_eq!(
            client
                .get(format!("{owner_url}/api/local/discord-credentials"))
                .bearer_auth(&session.token)
                .header("x-relay-local-control", CONTROL)
                .header("x-relay-instance-id", state.instance_id())
                .header("x-relay-server-id", state.server_id().unwrap())
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::OK
        );
        let health: Value = client
            .get(format!("{peer_url}/api/health"))
            .header("x-relay-network-ingress", INGRESS)
            .header("x-relay-network-peer", "100.64.0.7")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(!health.to_string().contains(INGRESS));
        assert!(!health.to_string().contains(CONTROL));
        assert_eq!(
            client
                .get(format!("{peer_url}/api/bootstrap"))
                .header("x-relay-network-ingress", INGRESS)
                .header("x-relay-network-peer", "100.64.0.7")
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
        let bootstrap: Value = client
            .get(format!("{peer_url}/api/bootstrap"))
            .header("x-relay-network-ingress", INGRESS)
            .header("x-relay-network-peer", "100.64.0.7")
            .bearer_auth(&session.token)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(!bootstrap.to_string().contains(INGRESS));
        assert!(!bootstrap.to_string().contains(CONTROL));
        state.request_shutdown();
        for task in [owner_task, peer_task] {
            tokio::time::timeout(std::time::Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
    }

    #[test]
    fn tailscale_mode_keeps_http_control_local_and_rejects_inconsistent_udp_bindings() {
        let defaults =
            Options::try_parse_from(["relay-server", "--tailscale-bind", "100.64.0.1"]).unwrap();
        let udp = resolved_udp_bind(&defaults).unwrap();
        assert_eq!(udp, "0.0.0.0".parse::<IpAddr>().unwrap());
        assert_eq!(
            managed_control_address(defaults.bind, defaults.port),
            "127.0.0.1:4580".parse::<SocketAddr>().unwrap()
        );
        assert!(requires_admin_initialization(defaults.bind, udp));
        let explicit = Options::try_parse_from([
            "relay-server",
            "--tailscale-bind",
            "100.64.0.1",
            "--udp-bind",
            "0.0.0.0",
        ])
        .unwrap();
        assert_eq!(resolved_udp_bind(&explicit).unwrap(), udp);
        for (flag, address) in [
            ("--bind", "0.0.0.0"),
            ("--bind", "100.64.0.1"),
            ("--bind", "::1"),
            ("--udp-bind", "127.0.0.1"),
            ("--udp-bind", "100.64.0.1"),
            ("--udp-bind", "::"),
        ] {
            let options = Options::try_parse_from([
                "relay-server",
                "--tailscale-bind",
                "100.64.0.1",
                flag,
                address,
            ])
            .unwrap();
            assert!(resolved_udp_bind(&options).is_err());
        }
    }

    #[tokio::test]
    async fn tailscale_hosting_cannot_create_the_first_owner_or_bind_without_a_community() {
        let fixture = tempfile::tempdir().unwrap();
        let missing = fixture.path().join("missing-tailscale-community");
        let options = Options::try_parse_from([
            "relay-server",
            "--tailscale-bind",
            "100.64.0.1",
            "--data",
            missing.to_str().unwrap(),
        ])
        .unwrap();
        let error = run(options).await.unwrap_err().to_string();
        assert!(error.contains("create its owner account"));
        assert!(!missing.exists());
        assert_eq!(std::fs::read_dir(fixture.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn two_http_listeners_share_the_server_and_shutdown_together() {
        let data = tempfile::tempdir().unwrap();
        let state = AppState::open(data.path(), "dual HTTP fixture", 0).unwrap();
        let first = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let first_address = first.local_addr().unwrap();
        let second = tokio::net::TcpListener::bind(SocketAddr::new(
            "127.0.0.2".parse().unwrap(),
            first_address.port(),
        ))
        .await
        .unwrap();
        let second_address = second.local_addr().unwrap();
        let first_task = tokio::spawn(serve_http(first, state.clone()));
        let second_task = tokio::spawn(serve_http(second, state.clone()));
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        for address in [first_address, second_address] {
            let health: Value = client
                .get(format!("http://{address}/api/health"))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(health["instance_id"], state.instance_id());
            assert_eq!(health["server_id"], state.server_id().unwrap());
        }
        state.request_shutdown();
        for task in [first_task, second_task] {
            tokio::time::timeout(std::time::Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
    }

    #[tokio::test]
    async fn tailscale_http_peers_cannot_use_private_local_controls_or_credentials() {
        use axum::{body::Body, extract::ConnectInfo, http::Request};
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let data = tempfile::tempdir().unwrap();
        let state = AppState::open_with_config(
            data.path(),
            "private host fixture",
            0,
            ServerConfig {
                local_control_token: Some("fixture-private-process-proof".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let app = router(state.clone());
        // Use a genuine owner session: otherwise ordinary authentication
        // middleware rejects credentials requests before the host-only guard.
        let registration = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/register")
                    .header("host", "127.0.0.1:4580")
                    .header("content-type", "application/json")
                    .extension(ConnectInfo("127.0.0.1:54320".parse::<SocketAddr>().unwrap()))
                    .body(Body::from(
                        json!({"username":"FixtureOwner","password":"fixture-owner-password-123","invite":""}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(registration.status(), axum::http::StatusCode::OK);
        let session: relay_protocol::Session =
            serde_json::from_slice(&registration.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        let source: SocketAddr = "100.64.0.7:54321".parse().unwrap();
        for (method, path, bearer, body, loopback_status) in [
            (
                "GET",
                "/api/local/discord-credentials",
                session.token.as_str(),
                String::new(),
                axum::http::StatusCode::OK,
            ),
            (
                "POST",
                "/api/local/shutdown",
                "fixture-private-process-proof",
                json!({"instance_id":state.instance_id(),"confirm_server_id":state.server_id().unwrap()}).to_string(),
                axum::http::StatusCode::NO_CONTENT,
            ),
        ] {
            for (peer, expected) in [
                (source, axum::http::StatusCode::FORBIDDEN),
                ("127.0.0.1:54322".parse().unwrap(), loopback_status),
            ] {
                let request = Request::builder()
                    .method(method)
                    .uri(path)
                    .header("host", "127.0.0.1:4580")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {bearer}"))
                    .header("x-relay-local-control", "fixture-private-process-proof")
                    .header("x-relay-instance-id", state.instance_id())
                    .header("x-relay-server-id", state.server_id().unwrap())
                    .extension(ConnectInfo(peer))
                    .body(Body::from(body.clone()))
                    .unwrap();
                let response = app.clone().oneshot(request).await.unwrap();
                assert_eq!(response.status(), expected, "{path} from {peer}");
                if peer == source {
                    assert!(tokio::time::timeout(
                        std::time::Duration::from_millis(20),
                        state.shutdown_requested()
                    )
                    .await
                    .is_err());
                }
            }
        }
    }

    #[test]
    fn udp_bind_defaults_to_the_http_interface() {
        let defaults = Options::try_parse_from(["relay-server"]).unwrap();
        assert_eq!(defaults.bind, "127.0.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(defaults.udp_bind.unwrap_or(defaults.bind), defaults.bind);
        for http_bind in ["127.0.0.1", "::1", "0.0.0.0", "::"] {
            let options = Options::try_parse_from(["relay-server", "--bind", http_bind]).unwrap();
            assert_eq!(options.udp_bind, None);
            assert_eq!(options.udp_bind.unwrap_or(options.bind), options.bind);
        }
    }

    #[test]
    fn separate_udp_interface_does_not_change_http_or_managed_control() {
        let options = Options::try_parse_from([
            "relay-server",
            "--bind",
            "127.0.0.1",
            "--udp-bind",
            "0.0.0.0",
        ])
        .unwrap();
        let udp_bind = options.udp_bind.unwrap_or(options.bind);
        assert_eq!(udp_bind, "0.0.0.0".parse::<IpAddr>().unwrap());
        assert!(options.bind.is_loopback());
        assert_eq!(
            managed_control_address(options.bind, options.port),
            "127.0.0.1:4580".parse::<SocketAddr>().unwrap()
        );
        assert!(requires_admin_initialization(options.bind, udp_bind));
    }

    #[test]
    fn administrator_guard_covers_each_nonloopback_transport() {
        let loopbacks: [IpAddr; 2] = ["127.0.0.1".parse().unwrap(), "::1".parse().unwrap()];
        let external: [IpAddr; 4] = [
            "0.0.0.0".parse().unwrap(),
            "192.168.1.20".parse().unwrap(),
            "::".parse().unwrap(),
            "2001:db8::1".parse().unwrap(),
        ];
        for http in loopbacks {
            for udp in loopbacks {
                assert!(!requires_admin_initialization(http, udp));
            }
            for udp in external {
                assert!(requires_admin_initialization(http, udp));
                assert!(requires_admin_initialization(udp, http));
            }
        }
    }

    #[test]
    fn managed_control_uses_http_bind_address_family() {
        for (http_bind, expected) in [
            ("0.0.0.0", "127.0.0.1:4580"),
            ("::", "[::1]:4580"),
            ("::1", "[::1]:4580"),
        ] {
            assert_eq!(
                managed_control_address(http_bind.parse().unwrap(), 4580),
                expected.parse::<SocketAddr>().unwrap()
            );
        }
    }

    #[test]
    fn udp_bind_rejects_hostnames_and_socket_addresses() {
        for invalid in ["localhost", "127.0.0.1:4581", "[::1]:4581"] {
            assert!(Options::try_parse_from(["relay-server", "--udp-bind", invalid]).is_err());
        }
    }
}

#[cfg(all(test, unix))]
mod voice_priority_tests {
    #[tokio::test]
    async fn voice_udp_uses_ef_classification() {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        super::mark_voice_priority(&socket);
        assert_eq!(socket2::SockRef::from(&socket).tos_v4().unwrap(), 0xB8);
    }
}
