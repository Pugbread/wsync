//! The live command surface: read-only inspection and diagnostics against a
//! connected Studio session, the conflict/decision commands, the mutating
//! write surface, the path tools, and the artifact surfaces — clipboard and
//! capture (Design §10.2, Appendix A — "Live inspection", "Live
//! diagnostics", "Conflict resolution", "Live writes", "Path tools",
//! "Studio clipboard", "Agent runtime").
//!
//! Every command here shares `super::client`'s discovery, the 5 s remote-op
//! timeout, and the `--raw` conventions documented there. The write family is
//! separated into its own modules because it carries the guardrails
//! (Design §10.6) that the read-only surface has no equivalent of; the
//! artifact surfaces share `transfer`'s verified chunk pump.

pub(crate) mod capture;
mod clipboard;
mod conflict;
mod diagnostics;
mod history;
mod inspect;
mod pathtools;
pub(crate) mod playtest;
mod reflect;
mod search;
mod session;
mod snapshot;
mod source;
mod transfer;
mod transmit;
mod write;

pub use capture::Capture;
pub use clipboard::{Copy, Paste};
pub use conflict::{Changes, Conflicts, Decision, Diff, Resolve};
pub use diagnostics::{Capabilities, Doctor, Ping, Status, Version};
pub use history::{Redo, Save, Undo, Waypoint};
pub use inspect::{Get, Ls, Props, Services, Tree};
pub use pathtools::{Meta, Path, Where};
pub use playtest::Playtest;
pub use reflect::{ClassInfo, Enum, Enums};
pub use search::{Find, FindAttr, Query};
pub use session::{Logs, Open, Select, Tail};
pub use snapshot::Snapshot;
pub use source::Source;
pub use transmit::Transmit;
pub use write::{Attr, Call, Eval, Mv, New, Rm, Set, Tag};
