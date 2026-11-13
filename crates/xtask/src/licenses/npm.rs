/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The licenses of the web packages' npm dependencies, read from their lockfiles.
//!
//! A package the lockfile marks `dev` or `devOptional` is reached only through
//! `devDependencies` and runs at build time; every other package is what `npm ci --omit=dev`
//! installs, which is where the shipped bundle's code comes from, so it may carry no copyleft
//! and no LGPL.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;

use serde::de::{Deserialize, Deserializer, IgnoredAny, MapAccess, Visitor};
use serde_json::Value;

use super::spdx::{self, Rank};
use super::table;
use crate::repo;

const ADDONS: [&str; 2] = ["web/selkies-web-core", "web/selkies-dashboard"];

/// The `packages` of a `package-lock.json` in the file's order: where one name is installed
/// at several paths, the page lists the license of the first.
struct Lock(Vec<(String, Value)>);

impl<'de> Deserialize<'de> for Lock {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Ordered;
        impl<'de> Visitor<'de> for Ordered {
            type Value = Vec<(String, Value)>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry()? {
                    entries.push(entry);
                }
                Ok(entries)
            }
        }
        struct Packages(Vec<(String, Value)>);
        impl<'de> Deserialize<'de> for Packages {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                deserializer.deserialize_map(Ordered).map(Self)
            }
        }
        struct Document;
        impl<'de> Visitor<'de> for Document {
            type Value = Lock;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a package-lock.json object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Lock, A::Error> {
                let mut packages = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    if key == "packages" {
                        packages = map.next_value::<Packages>()?.0;
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(Lock(packages))
            }
        }
        deserializer.deserialize_map(Document)
    }
}

/// Whether a lockfile flag is set to anything but false, null, zero, or nothing.
fn flagged(entry: &Value, flag: &str) -> bool {
    !matches!(&entry[flag], Value::Null | Value::Bool(false))
        && entry[flag] != 0
        && entry[flag] != ""
}

/// The table of the web packages' direct dependencies and the table of every license each
/// scope pulls in, with what is unlicensed, unclassified, or copyleft at run time added to
/// `problems`.
pub fn audit(problems: &mut Vec<String>) -> Result<(String, String), String> {
    let mut direct_rows = Vec::new();
    let mut scope_rows = Vec::new();
    for addon in ADDONS {
        let read = |file: &str| {
            fs::read_to_string(repo().join(addon).join(file))
                .map_err(|e| format!("{addon}/{file}: {e}"))
        };
        let lock: Lock = serde_json::from_str(&read("package-lock.json")?)
            .map_err(|e| format!("{addon}/package-lock.json: {e}"))?;
        let manifest: Value = serde_json::from_str(&read("package.json")?)
            .map_err(|e| format!("{addon}/package.json: {e}"))?;
        let mut scopes = [
            ("dependencies", BTreeMap::new()),
            ("devDependencies", BTreeMap::new()),
        ];
        let mut licenses: BTreeMap<&str, String> = BTreeMap::new();
        for (path, entry) in &lock.0 {
            if path.is_empty() || flagged(entry, "link") {
                continue;
            }
            let name = path
                .rsplit_once("node_modules/")
                .map_or(path.as_str(), |(_, name)| name);
            let dev = flagged(entry, "dev") || flagged(entry, "devOptional");
            let Some(published) = entry["license"]
                .as_str()
                .filter(|license| !license.is_empty())
            else {
                problems.push(format!(
                    "{addon} {name}: no machine-readable license in package-lock.json ({})",
                    entry["license"]
                ));
                continue;
            };
            let expr = match spdx::parse(published) {
                Ok(expr) => expr,
                Err(error) => {
                    problems.push(format!("{addon} {name}: {error}"));
                    continue;
                }
            };
            let (rank, text) = (expr.rank(), expr.render());
            licenses.entry(name).or_insert_with(|| text.clone());
            if rank == Rank::Unknown {
                problems.push(format!(
                    "{addon} {name}: license '{published}' is not classified"
                ));
            } else if matches!(rank, Rank::Weak | Rank::Copyleft) && !dev {
                problems.push(format!(
                    "{addon} {name}: {} license '{published}' among the runtime dependencies",
                    rank.label()
                ));
            }
            let worst = scopes[usize::from(dev)]
                .1
                .entry(text)
                .or_insert(Rank::Permissive);
            *worst = rank.max(*worst);
        }
        let dependencies = manifest["dependencies"].as_object();
        let mut names: Vec<&String> = dependencies
            .into_iter()
            .flat_map(|dependencies| dependencies.keys())
            .collect();
        names.sort();
        for name in names {
            let license = licenses
                .get(name.as_str())
                .map_or("(not in package-lock.json)", String::as_str);
            direct_rows.push(vec![
                format!("`{addon}`"),
                format!("`{name}`"),
                license.to_string(),
            ]);
        }
        for (scope, found) in scopes {
            if found.is_empty() {
                continue;
            }
            let listed: Vec<String> = found
                .iter()
                .map(|(text, rank)| match rank {
                    Rank::Permissive => text.clone(),
                    rank => format!("{text} ({})", rank.label()),
                })
                .collect();
            scope_rows.push(vec![
                format!("`{addon}`"),
                format!("`{scope}` and what they pull in"),
                listed.join(", "),
            ]);
        }
    }
    Ok((
        table(&["Addon", "Package", "License"], &direct_rows),
        table(&["Addon", "Packages", "Licenses"], &scope_rows),
    ))
}
