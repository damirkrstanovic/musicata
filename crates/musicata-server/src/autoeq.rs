// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Musicata — a local-first music server + web controller.
// Copyright (C) 2026 Damir Krstanović
//
// This program is free software: you can redistribute it and/or modify it under the terms of
// the GNU Affero General Public License as published by the Free Software Foundation, either
// version 3 of the License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY;
// without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
// See the GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License along with this
// program. If not, see <https://www.gnu.org/licenses/>.

//! URL construction for the AutoEq headphone-correction proxy.
//!
//! The browser cannot reach `raw.githubusercontent.com` (the CSP allows `'self'` only), so it
//! asks Musicata for the model index and for a chosen model's ParametricEQ file, and the server
//! fetches them (see [`crate::proxy`]). Presets are relayed, never stored — a Musicata instance
//! holds no copy of AutoEq's corpus.
//!
//! The security property this module exists to guarantee: **the client names a model, never a
//! URL.** The base is a constant, the caller supplies only a path *suffix*, and
//! [`preset_url`] rejects anything that could escape that suffix into a different host, a
//! parent directory, or a query of its own. Without that check the endpoint would be an open
//! proxy sitting inside the user's LAN.

/// AutoEq publishes its computed results here; `INDEX.md` lists every model.
const RESULTS_BASE: &str = "https://raw.githubusercontent.com/jaakkopasanen/AutoEq/master/results";

/// The model index — one line per measured headphone.
pub fn index_url() -> String {
    format!("{RESULTS_BASE}/INDEX.md")
}

/// The ParametricEQ file for `path`, which is a `results/`-relative directory exactly as
/// `INDEX.md` spells it (already percent-encoded, e.g. `oratory1990/over-ear/Sennheiser%20HD%20600`).
///
/// AutoEq names the file after the directory's last segment, so the caller supplies only the
/// directory and the filename is derived here — the client never gets to choose it.
pub fn preset_url(path: &str) -> anyhow::Result<String> {
    // Check the caller's string BEFORE normalising it. Trimming first would let `//evil/x`
    // through: the trim would eat the leading pair and the `//` test would then see nothing.
    // Each of these would let the suffix escape the fixed base: `..` climbs out of `results/`,
    // `://` and a leading `//` reroute to another host, and `?`/`#` truncate the path so the
    // derived filename is dropped. A backslash is normalised to `/` by some proxies, and
    // control characters allow request smuggling.
    for forbidden in ["..", "://", "//", "?", "#", "\\"] {
        if path.contains(forbidden) {
            anyhow::bail!("AutoEq path may not contain {forbidden:?}");
        }
    }
    if path.chars().any(|c| c.is_control()) {
        anyhow::bail!("AutoEq path may not contain control characters");
    }

    let path = path.trim_matches('/');
    if path.is_empty() {
        anyhow::bail!("empty AutoEq path");
    }

    let model = path
        .rsplit('/')
        .next()
        .filter(|segment| !segment.is_empty())
        .ok_or_else(|| anyhow::anyhow!("AutoEq path has no model segment"))?;
    Ok(format!("{RESULTS_BASE}/{path}/{model}%20ParametricEQ.txt"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_the_preset_url_from_the_directory() {
        assert_eq!(
            preset_url("oratory1990/over-ear/Sennheiser%20HD%20600").unwrap(),
            format!(
                "{RESULTS_BASE}/oratory1990/over-ear/Sennheiser%20HD%20600/Sennheiser%20HD%20600%20ParametricEQ.txt"
            )
        );
    }

    #[test]
    fn tolerates_surrounding_slashes() {
        let expected = preset_url("DHRME/in-ear/1MORE%20Aero").unwrap();
        assert_eq!(preset_url("/DHRME/in-ear/1MORE%20Aero/").unwrap(), expected);
    }

    /// The whole point of the module: none of these may produce a URL.
    #[test]
    fn refuses_anything_that_escapes_the_base() {
        for path in [
            "",
            "/",
            "../../../etc/passwd",
            "a/../../b",
            "https://evil.example/x",
            "//evil.example/x",
            "model?x=1",
            "model#frag",
            "model\\..\\x",
            "model\u{0}name",
            "model\nname",
        ] {
            assert!(
                preset_url(path).is_err(),
                "should have refused {path:?} but got {:?}",
                preset_url(path)
            );
        }
    }

    /// Real AutoEq model names carry spaces (percent-encoded), parentheses and non-ASCII —
    /// the guard must not reject those.
    #[test]
    fn accepts_real_model_names() {
        for path in [
            "oratory1990/over-ear/Sennheiser%20HD%20600",
            "DHRME/in-ear/1MORE%20Aero%20(ANC%20Off)",
            "crinacle/GRAS%2043AG-7%20over-ear/Focal%20Clear",
            "rtings/over-ear/Beyerdynamic%20DT%20990%20PRO",
        ] {
            assert!(preset_url(path).is_ok(), "should have accepted {path:?}");
        }
    }

    #[test]
    fn index_is_under_the_same_base() {
        assert!(index_url().starts_with(RESULTS_BASE));
        assert!(index_url().ends_with("/INDEX.md"));
    }
}
