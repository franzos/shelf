//! Semantic checks layered on top of serde parsing.
//!
//! Validation collects every problem rather than bailing on the first.

use std::collections::{BTreeMap, BTreeSet};

use globset::Glob;

use super::profile::Profile;
use crate::error::ValidationError;
use crate::template::Template;

/// Run every documented check against `profile`. Empty vec on success.
pub fn validate(profile: &Profile) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    if profile.inputs.is_empty() {
        errors.push(ValidationError::NoInputs);
    }

    if profile.outputs.is_empty() {
        errors.push(ValidationError::NoOutputs);
    }

    check_unique_output_names(profile, &mut errors);
    check_kind_extension_uniqueness(profile, &mut errors);
    check_globs(profile, &mut errors);
    check_templates(profile, &mut errors);

    errors
}

fn check_unique_output_names(profile: &Profile, errors: &mut Vec<ValidationError>) {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut reported: BTreeSet<&str> = BTreeSet::new();
    for o in &profile.outputs {
        // `reported` keeps us from flagging the same name more than once
        // if it appears three+ times.
        if !seen.insert(o.name.as_str()) && reported.insert(o.name.as_str()) {
            errors.push(ValidationError::DuplicateOutputName(o.name.clone()));
        }
    }
}

fn check_kind_extension_uniqueness(profile: &Profile, errors: &mut Vec<ValidationError>) {
    let mut owner: BTreeMap<String, String> = BTreeMap::new();
    for (kind, exts) in &profile.kinds {
        for ext in exts {
            let key = ext.to_ascii_lowercase();
            if let Some(prev) = owner.get(&key) {
                if prev != kind {
                    errors.push(ValidationError::ExtensionInMultipleKinds {
                        ext: key.clone(),
                        first: prev.clone(),
                        second: kind.clone(),
                    });
                }
            } else {
                owner.insert(key, kind.clone());
            }
        }
    }
}

fn check_globs(profile: &Profile, errors: &mut Vec<ValidationError>) {
    let mut check = |patterns: &[String], location: String| {
        for pat in patterns {
            if let Err(e) = Glob::new(pat) {
                errors.push(ValidationError::BadGlob {
                    location: location.clone(),
                    pattern: pat.clone(),
                    reason: e.to_string(),
                });
            }
        }
    };

    check(&profile.filters.include, "filters.include".into());
    check(&profile.filters.exclude, "filters.exclude".into());

    for out in &profile.outputs {
        if let Some(patterns) = &out.match_ {
            check(patterns, format!("output.{}.match", out.name));
        }
    }
}

fn check_templates(profile: &Profile, errors: &mut Vec<ValidationError>) {
    let mut check = |tpl: &str, location: String| {
        if let Err(e) = Template::parse(tpl) {
            errors.push(ValidationError::BadTemplate {
                location,
                template: tpl.to_string(),
                reason: e.to_string(),
            });
        }
    };

    for out in &profile.outputs {
        check(&out.directory, format!("output.{}.directory", out.name));
        check(&out.filename, format!("output.{}.filename", out.name));
        for (kind, tpl) in &out.directory_for {
            check(tpl, format!("output.{}.directory_for.{}", out.name, kind));
        }
        for (kind, tpl) in &out.filename_for {
            check(tpl, format!("output.{}.filename_for.{}", out.name, kind));
        }
    }
}
