use std::{sync::OnceLock, time::Duration};

use crate::{core::meta::SyncRule, middleware::Middleware};

// Paths that should be ignored before they are even processed
// useful to save ton of computing time, however users won't
// be able to set them in `sync_rules` or project `$path`
pub const BLACKLISTED_PATHS: [&str; 1] = [".DS_Store"];

/// The `wsync/1` protocol version spoken by `GET /hello` and the WebSocket
/// surface (Design §5)
pub const PROTOCOL_VERSION: u8 = 1;

/// The Argon version reported by msgpack `GET /details` when `compat_argon`
/// is enabled, so a stock Argon plugin (which semver-gates against that
/// endpoint) accepts a WSync daemon during migration. Every other surface
/// (`GET /hello`, the WS hello) always reports WSync's real version
pub const ARGON_COMPAT_VERSION: &str = "2.0.29";

/// How long a WebSocket client has to deliver its `hello` frame before the
/// daemon closes the connection (Ro-Sync invariant: every connect has a
/// deadline)
pub const WS_HELLO_DEADLINE: Duration = Duration::from_secs(10);

/// Interval between server-sent WS heartbeat `ping` frames
pub const WS_PING_INTERVAL: Duration = Duration::from_secs(2);

/// A WS client that has not answered a `ping` with a `pong` (or any other
/// frame) for this long is disconnected
pub const WS_PONG_TIMEOUT: Duration = Duration::from_secs(8);

/// Outbound frame queue capacity for the plugin connection. Overflow means
/// the plugin stopped reading faster than the daemon syncs and results in a
/// typed `shutdown` (the plugin re-hydrates on reconnect)
pub const WS_PLUGIN_QUEUE: usize = 1024;

/// Outbound frame queue capacity for `watch`/`app`/`agent` clients. A slow
/// consumer is disconnected with a typed overload `shutdown` instead of ever
/// blocking the plugin bridge
pub const WS_WATCH_QUEUE: usize = 256;

/// Maximum number of top-level operations in one `sync` frame; larger change
/// sets are split into multiple frames (Design appendix C)
pub const SYNC_FRAME_MAX_OPS: usize = 256;

/// Soft byte limit for one `sync` frame. Frames above it are bisected while
/// they contain more than one operation (a single oversized op is sent whole)
pub const SYNC_FRAME_MAX_BYTES: usize = 512 * 1024;

/// Default `POST /request` remote-op timeout (Design §5.4)
pub const REQUEST_DEFAULT_TIMEOUT_MS: u64 = 5000;

/// Upper bound for a caller-supplied `timeoutMs` on `POST /request`
pub const REQUEST_MAX_TIMEOUT_MS: u64 = 600_000;

/// A long-poll (msgpack compat) plugin that has not polled `/read` for this
/// long is considered gone and its single-plugin slot can be reclaimed
/// (`QUEUE_TIMEOUT` is the poll re-issue interval, so allow two misses)
pub const LONGPOLL_EVICT_AFTER: Duration = Duration::from_secs(130);

// Current version of the project templates, this constant
// should be manually bumped when there are any changes
// made to the `assets/templates` directory
pub const TEMPLATES_VERSION: u8 = 4;

// Maximum payload size that can be sent from client
// to the server, usually containing changes to apply,
// currently it is 512 MiB but it is a huge overkill
pub const MAX_PAYLOAD_SIZE: usize = 536_870_912;

/// How long the server should wait for the changes to
/// appear in the queue before manually "timing out"
/// the client request and sending back an empty `Changes`
pub const QUEUE_TIMEOUT: Duration = Duration::from_secs(60);

// VFS events will be ignored for this amount of time
// after the last change that has been made by the client,
// this saves a lot of computing time
pub const SYNCBACK_DEBOUNCE_TIME: Duration = Duration::from_millis(300);

/// How long a propagated FS change keeps its previous baseline as the
/// conflict reference. The WSync plugin does not acknowledge applied `sync`
/// frames (its watcher deliberately suppresses echo pushes), so a Studio
/// `push` racing an in-flight FS edit is detected by holding the pre-edit
/// baseline for this window: a push arriving inside it that matches neither
/// the sent nor the prior content is a both-edited conflict. The window
/// comfortably covers the plugin's 0.5 s push aggregation plus delivery
pub const CONFLICT_RACE_WINDOW: Duration = Duration::from_secs(2);

/// Upper bound on simultaneously parked conflicts. Beyond it new conflicts
/// still exclude the losing operation (nothing is ever silently clobbered)
/// but are not stored or listed, and a warning is logged (Design §6.3:
/// parked conflicts are bounded)
pub const CONFLICT_PARK_LIMIT: usize = 512;

/// Largest script source carried inside one conflict record (`GET /resolve`)
/// or event payload; longer sources are cut and flagged `truncated: true`
pub const CONFLICT_SOURCE_CAP: usize = 256 * 1024;

/// Divergence upload (`POST /compare`) bounds: records per chunk and request
/// body bytes (Design §7.2 / Appendix C)
pub const COMPARE_CHUNK_MAX_ENTRIES: usize = 512;
pub const COMPARE_BODY_MAX_BYTES: usize = 512 * 1024;

/// `GET /choice/details` paging bounds: default and maximum records per page
/// plus the response byte budget (Design §5.2 / Appendix C)
pub const CHOICE_DETAILS_DEFAULT_LIMIT: usize = 512;
pub const CHOICE_DETAILS_MAX_LIMIT: usize = 1024;
pub const CHOICE_DETAILS_BYTE_BUDGET: usize = 512 * 1024;

/// `POST /choice/selection` bounds: ids per chunk and request body bytes
pub const SELECTION_CHUNK_MAX_IDS: usize = 2048;
pub const SELECTION_BODY_MAX_BYTES: usize = 64 * 1024;

/// Runtime directories WSync creates inside a served workspace. They are part
/// of the default ignore set (Design §12: gitignored runtime dirs) so their
/// churn never re-enters the sync pipeline
pub const RUNTIME_DIRS: [&str; 3] = [".wsync-backups", ".wsync-artifacts", ".wsync-workflows"];

/// The per-project backup root for fenced Keep-Studio applies (Design §7.4-A)
pub const BACKUPS_DIR: &str = ".wsync-backups";

/// Marker file inside a transfer's backup directory proving the transfer
/// completed; only marked directories are ever pruned
pub const BACKUP_COMPLETE_MARKER: &str = "complete.json";

/// Backup retention (Design §7.4-A): completed-transfer backups are pruned
/// once older than this many days or beyond this many newest transfers;
/// partial (unmarked) backups are never pruned
pub const BACKUP_KEEP_DAYS: i64 = 7;
pub const BACKUP_KEEP_COUNT: usize = 32;

/// Keep-Studio pull bounds (Design §7.4-A): chunk size for `source_read`,
/// per-script, per-root and per-transfer byte budgets
pub const SOURCE_READ_CHUNK_BYTES: u64 = 64 * 1024;
pub const BULK_SCRIPT_MAX_BYTES: u64 = 32 * 1024 * 1024;
pub const BULK_ROOT_MAX_BYTES: u64 = 64 * 1024 * 1024;
pub const BULK_TRANSFER_MAX_BYTES: u64 = 128 * 1024 * 1024;

/// Per-op deadline for the Keep-Studio pull ops (`read_subtree` /
/// `source_read`); bulk phases carry longer deadlines than live ops
pub const BULK_OP_TIMEOUT_MS: u64 = 30_000;

/// `writes.log` rotates to `writes.log.1` (one generation) at this size
/// (Design §6.4 / §12)
pub const WRITES_LOG_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// Result bound for the daemon-side `where` op (registry contract)
pub const WHERE_MATCH_LIMIT: usize = 500;

// Set of default sync rules that is used to determine
// what middleware should be used to process a file
// users can override these rules in the project file
pub fn default_sync_rules() -> &'static Vec<SyncRule> {
	static SYNC_RULES: OnceLock<Vec<SyncRule>> = OnceLock::new();

	SYNC_RULES.get_or_init(|| {
		vec![
			// Project and data files
			SyncRule::new(Middleware::Project)
				.with_pattern("*.project.json")
				.with_child_pattern("default.project.json"),
			SyncRule::new(Middleware::InstanceData)
				.with_pattern("*.data.json")
				.with_child_pattern(".data.json"),
			SyncRule::new(Middleware::InstanceData)
				.with_pattern("*.meta.json")
				.with_child_pattern("init.meta.json"),
			//////////////////////////////////////////////////////////////////////////////////////////
			// Luau scripts
			SyncRule::new(Middleware::ServerScript)
				.with_pattern("*.server.luau")
				.with_child_pattern("init.server.luau")
				.with_suffix(".server.luau"),
			SyncRule::new(Middleware::ClientScript)
				.with_pattern("*.client.luau")
				.with_child_pattern("init.client.luau")
				.with_suffix(".client.luau"),
			SyncRule::new(Middleware::LocalScript)
				.with_pattern("*.local.luau")
				.with_child_pattern("init.local.luau")
				.with_suffix(".local.luau"),
			SyncRule::new(Middleware::RunServerScript)
				.with_pattern("*.runserver.luau")
				.with_child_pattern("init.runserver.luau")
				.with_suffix(".runserver.luau"),
			SyncRule::new(Middleware::ModuleScript)
				.with_pattern("*.luau")
				.with_child_pattern("init.luau"),
			// Luau scripts for Argon Legacy
			SyncRule::new(Middleware::ServerScript)
				.with_pattern("*.server.luau")
				.with_child_pattern(".src.server.luau")
				.with_suffix(".server.luau")
				.with_exclude("init.server.luau"),
			SyncRule::new(Middleware::ClientScript)
				.with_pattern("*.client.luau")
				.with_child_pattern(".src.client.luau")
				.with_suffix(".client.luau")
				.with_exclude("init.client.luau"),
			SyncRule::new(Middleware::ModuleScript)
				.with_pattern("*.luau")
				.with_child_pattern(".src.luau")
				.with_exclude("init.luau"),
			//////////////////////////////////////////////////////////////////////////////////////////
			// Lua scripts
			SyncRule::new(Middleware::ServerScript)
				.with_pattern("*.server.lua")
				.with_child_pattern("init.server.lua")
				.with_suffix(".server.lua"),
			SyncRule::new(Middleware::ClientScript)
				.with_pattern("*.client.lua")
				.with_child_pattern("init.client.lua")
				.with_suffix(".client.lua"),
			SyncRule::new(Middleware::LocalScript)
				.with_pattern("*.local.lua")
				.with_child_pattern("init.local.lua")
				.with_suffix(".local.lua"),
			SyncRule::new(Middleware::RunServerScript)
				.with_pattern("*.runserver.lua")
				.with_child_pattern("init.runserver.lua")
				.with_suffix(".runserver.lua"),
			SyncRule::new(Middleware::ModuleScript)
				.with_pattern("*.lua")
				.with_child_pattern("init.lua"),
			// Lua scripts for Argon legacy
			SyncRule::new(Middleware::ServerScript)
				.with_pattern("*.server.lua")
				.with_child_pattern(".src.server.lua")
				.with_suffix(".server.lua")
				.with_exclude("init.server.lua"),
			SyncRule::new(Middleware::ClientScript)
				.with_pattern("*.client.lua")
				.with_child_pattern(".src.client.lua")
				.with_suffix(".client.lua")
				.with_exclude("init.client.lua"),
			SyncRule::new(Middleware::ModuleScript)
				.with_pattern("*.lua")
				.with_child_pattern(".src.lua")
				.with_exclude("init.lua"),
			//////////////////////////////////////////////////////////////////////////////////////////
			// Other file types
			SyncRule::new(Middleware::StringValue)
				.with_pattern("*.txt")
				.with_child_pattern("init.txt"),
			SyncRule::new(Middleware::RichStringValue)
				.with_pattern("*.md")
				.with_child_pattern("init.md"),
			SyncRule::new(Middleware::LocalizationTable)
				.with_pattern("*.csv")
				.with_child_pattern("init.csv"),
			SyncRule::new(Middleware::JsonModule)
				.with_pattern("*.json")
				.with_child_pattern("init.json")
				.with_excludes(&["*.model.json", "*.data.json", "*.meta.json"]),
			SyncRule::new(Middleware::TomlModule)
				.with_pattern("*.toml")
				.with_child_pattern("init.toml"),
			SyncRule::new(Middleware::YamlModule)
				.with_pattern("*.yaml")
				.with_child_pattern("init.yaml"),
			SyncRule::new(Middleware::YamlModule)
				.with_pattern("*.yml")
				.with_child_pattern("init.yml"),
			SyncRule::new(Middleware::MsgpackModule)
				.with_pattern("*.msgpack")
				.with_child_pattern("init.msgpack"),
			// Model files
			SyncRule::new(Middleware::JsonModel)
				.with_pattern("*.model.json")
				.with_child_pattern("init.model.json")
				.with_suffix(".model.json"),
			SyncRule::new(Middleware::RbxmModel)
				.with_pattern("*.rbxm")
				.with_child_pattern("init.rbxm"),
			SyncRule::new(Middleware::RbxmxModel)
				.with_pattern("*.rbxmx")
				.with_child_pattern("init.rbxmx"),
		]
	})
}
