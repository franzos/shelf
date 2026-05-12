//! Profile loading, discovery, and validation.

pub(crate) mod discovery;
pub(crate) mod profile;
pub(crate) mod validate;

pub use discovery::{ProfileEntry, default_config_dir, discover_profiles, resolve_profile};
pub use profile::{
    Dedupe, DedupeScope, DedupeStrategy, Extensions, Filters, Health, Metadata, OnConflict,
    OnDuplicate, OpMode, Output, Profile, Sequence, SequenceScope, StateCfg, TemplatesConfig,
    load_profile,
};
