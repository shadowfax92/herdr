//! Popup sizes from `[popup.size]` and `[[popup.profile]]`, generalized from the
//! old comments, scratch and talon width profiles.
//!
//! A *popup key* names one popup's size slot (`find`, `comment-capture`,
//! `scratch-nvim`, …). It is not always the manifest pane id: every scratch shares
//! the `scratch` pane but has its own key. Resolution for a key and the client's
//! width in cells:
//!
//! 1. the first `[[popup.profile]]` whose width range matches **and** that sizes
//!    this key;
//! 2. else `[popup.size].<key>`;
//! 3. else nothing, and the caller omits width/height so Herdr applies the
//!    manifest pane's own default (Herdr falls back per dimension).
//!
//! Skipping a matching profile that does not size the key keeps one merged file
//! equivalent to the per-plugin files it replaced: each old plugin's profiles
//! sized every one of its popups, so step 1 still picks the same profile for
//! them, and one mode's profiles never shadow another mode's keys.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use anyhow::{bail, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// One popup dimension: whole terminal cells, or a percentage of Herdr's pane area.
///
/// Serializes like Herdr's own manifest field: an integer is cells, a string
/// `"N%"` is a percentage. Cells must be positive and percentages 1–100.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopupSize {
    Cells(u16),
    Percent(u8),
}

impl PopupSize {
    fn validated_cells(cells: u64) -> Result<Self, String> {
        match u16::try_from(cells) {
            Ok(cells) if cells > 0 => Ok(Self::Cells(cells)),
            _ => Err(format!(
                "popup size {cells} must be 1-65535 cells or a percentage like \"80%\""
            )),
        }
    }

    fn validated_percent(text: &str) -> Result<Self, String> {
        match text.parse::<u8>() {
            Ok(percent) if (1..=100).contains(&percent) => Ok(Self::Percent(percent)),
            _ => Err(format!(
                "popup size \"{text}%\" must be between 1% and 100%"
            )),
        }
    }
}

/// Parses `"80"` (cells) or `"80%"`, the old YAML/CLI spelling. Migrators use
/// this; the TOML config itself spells cells as a bare integer.
impl FromStr for PopupSize {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        if let Some(percent) = value.strip_suffix('%') {
            return Self::validated_percent(percent);
        }
        value
            .parse::<u64>()
            .map_err(|_| format!("popup size \"{value}\" must be cells or a percentage like 80%"))
            .and_then(Self::validated_cells)
    }
}

/// `80` or `80%`: the spelling `herdr plugin pane open --width` accepts.
impl fmt::Display for PopupSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cells(cells) => write!(f, "{cells}"),
            Self::Percent(percent) => write!(f, "{percent}%"),
        }
    }
}

impl Serialize for PopupSize {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Cells(cells) => serializer.serialize_u16(*cells),
            Self::Percent(percent) => serializer.serialize_str(&format!("{percent}%")),
        }
    }
}

impl<'de> Deserialize<'de> for PopupSize {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = PopupSize;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a cell count like 80 or a percentage string like \"80%\"")
            }

            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<PopupSize, E> {
                PopupSize::validated_cells(value).map_err(E::custom)
            }

            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<PopupSize, E> {
                u64::try_from(value)
                    .map_err(|_| E::custom(format!("popup size {value} must be positive")))
                    .and_then(|value| PopupSize::validated_cells(value).map_err(E::custom))
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<PopupSize, E> {
                match value.strip_suffix('%') {
                    Some(percent) => PopupSize::validated_percent(percent).map_err(E::custom),
                    None => Err(E::custom(format!(
                        "popup size \"{value}\": strings must be percentages like \"80%\"; write cells as a number"
                    ))),
                }
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

/// A popup's outer width and height, as `{ width = 110, height = "90%" }`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PopupDims {
    pub width: PopupSize,
    pub height: PopupSize,
}

/// One `[[popup.profile]]`: sizes that apply while the client is `min..=max` cells wide.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PopupProfile {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_client_width: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_client_width: Option<u16>,
    /// Popup key → size while this profile matches.
    #[serde(default)]
    pub size: BTreeMap<String, PopupDims>,
}

impl PopupProfile {
    /// Whether a client `width` cells wide falls inside this profile's range.
    pub fn matches(&self, width: u16) -> bool {
        self.min_client_width.is_none_or(|min| width >= min)
            && self.max_client_width.is_none_or(|max| width <= max)
    }
}

/// The whole `[popup]` table.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PopupConfig {
    /// Popup key → size used when no profile sizes the key.
    #[serde(default)]
    pub size: BTreeMap<String, PopupDims>,
    /// Width profiles in priority order.
    #[serde(default)]
    pub profile: Vec<PopupProfile>,
}

/// A size taken from config, with the profile that supplied it (None: `[popup.size]`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedPopup {
    pub dims: PopupDims,
    pub profile: Option<String>,
}

impl PopupConfig {
    /// Rejects empty profile names and inverted width ranges. Sizes are already
    /// range-checked by [`PopupSize`]'s deserializer.
    pub fn validate(&self) -> Result<()> {
        for profile in &self.profile {
            if profile.name.trim().is_empty() {
                bail!("[[popup.profile]] name cannot be empty");
            }
            if let (Some(min), Some(max)) = (profile.min_client_width, profile.max_client_width) {
                if min > max {
                    bail!(
                        "popup profile `{}` has min_client_width {min} above max_client_width {max}",
                        profile.name
                    );
                }
            }
        }
        Ok(())
    }

    /// The configured size for `key` on a client `client_width` cells wide, or
    /// None when config leaves it to the manifest default. An unknown width
    /// (the layout could not be read) skips profiles entirely.
    pub fn resolve(&self, key: &str, client_width: Option<u16>) -> Option<ResolvedPopup> {
        let from_profile = client_width.and_then(|width| {
            self.profile
                .iter()
                .filter(|profile| profile.matches(width))
                .find_map(|profile| {
                    profile.size.get(key).map(|dims| ResolvedPopup {
                        dims: *dims,
                        profile: Some(profile.name.clone()),
                    })
                })
        });
        from_profile.or_else(|| {
            self.size.get(key).map(|dims| ResolvedPopup {
                dims: *dims,
                profile: None,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dims(width: &str, height: &str) -> PopupDims {
        PopupDims {
            width: width.parse().unwrap(),
            height: height.parse().unwrap(),
        }
    }

    fn parse(source: &str) -> Result<PopupConfig, toml::de::Error> {
        toml::from_str(source)
    }

    /// The old comments/scratch default profiles, as the migrated TOML spells them.
    const RESPONSIVE: &str = r#"
size.comment-capture = { width = "90%", height = "90%" }
size.comment-review = { width = "90%", height = "85%" }

[[profile]]
name = "laptop"
max_client_width = 310
size.comment-capture = { width = "95%", height = "90%" }
size.comment-review = { width = "95%", height = "85%" }

[[profile]]
name = "2-3-ultrawide"
max_client_width = 350
size.comment-capture = { width = "90%", height = "90%" }
size.comment-review = { width = "90%", height = "85%" }

[[profile]]
name = "full-ultrawide"
min_client_width = 400
size.comment-capture = { width = "50%", height = "70%" }
size.comment-review = { width = "70%", height = "85%" }
"#;

    #[test]
    fn sizes_accept_cells_and_percentages_and_round_trip() {
        let config = parse("size.find = { width = 110, height = \"90%\" }").unwrap();
        let find = config.size["find"];
        assert_eq!(find.width, PopupSize::Cells(110));
        assert_eq!(find.height, PopupSize::Percent(90));
        assert_eq!(find.width.to_string(), "110");
        assert_eq!(find.height.to_string(), "90%");
        assert_eq!(
            serde_json::to_value(find).unwrap(),
            serde_json::json!({"width": 110, "height": "90%"})
        );
        assert_eq!("80".parse::<PopupSize>(), Ok(PopupSize::Cells(80)));
        assert_eq!(" 80% ".parse::<PopupSize>(), Ok(PopupSize::Percent(80)));
    }

    #[test]
    fn invalid_sizes_are_rejected() {
        for bad in [
            "size.find = { width = 0, height = 10 }",
            "size.find = { width = 70000, height = 10 }",
            "size.find = { width = -3, height = 10 }",
            "size.find = { width = \"120%\", height = 10 }",
            "size.find = { width = \"0%\", height = 10 }",
            "size.find = { width = \"80\", height = 10 }",
            "size.find = { width = 80 }",
            "size.find = { width = 80, height = 10, depth = 3 }",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        assert!("120%".parse::<PopupSize>().is_err());
        assert!("wide".parse::<PopupSize>().is_err());
    }

    #[test]
    fn responsive_profiles_select_in_order() {
        let config = parse(RESPONSIVE).unwrap();
        config.validate().unwrap();
        let capture = |width| config.resolve("comment-capture", width).unwrap();

        assert_eq!(capture(Some(300)).dims, dims("95%", "90%"));
        assert_eq!(capture(Some(300)).profile.as_deref(), Some("laptop"));
        assert_eq!(capture(Some(330)).dims, dims("90%", "90%"));
        // No profile covers 351..=399: the base size applies.
        assert_eq!(capture(Some(380)).dims, dims("90%", "90%"));
        assert_eq!(capture(Some(380)).profile, None);
        assert_eq!(capture(Some(512)).dims, dims("50%", "70%"));
        assert_eq!(
            config.resolve("comment-review", Some(512)).unwrap().dims,
            dims("70%", "85%")
        );
        // Without a client width only the base size can apply.
        assert_eq!(capture(None).dims, dims("90%", "90%"));
    }

    #[test]
    fn a_matching_profile_without_the_key_does_not_shadow_a_later_one() {
        let config = parse(
            r#"
[[profile]]
name = "scratch-laptop"
max_client_width = 310
size.scratch-nvim = { width = "95%", height = "95%" }

[[profile]]
name = "history-laptop"
max_client_width = 310
size.history = { width = "95%", height = "90%" }
"#,
        )
        .unwrap();

        let history = config.resolve("history", Some(300)).unwrap();
        assert_eq!(history.dims, dims("95%", "90%"));
        assert_eq!(history.profile.as_deref(), Some("history-laptop"));
    }

    #[test]
    fn unconfigured_keys_fall_through_to_the_manifest_default() {
        let config = parse(RESPONSIVE).unwrap();
        assert_eq!(config.resolve("find", Some(300)), None);
        assert_eq!(PopupConfig::default().resolve("find", None), None);
    }

    #[test]
    fn invalid_profiles_are_rejected() {
        let inverted =
            parse("[[profile]]\nname = \"x\"\nmin_client_width = 400\nmax_client_width = 300\n")
                .unwrap();
        assert!(inverted
            .validate()
            .unwrap_err()
            .to_string()
            .contains("min_client_width"));
        let unnamed = parse("[[profile]]\nname = \" \"\n").unwrap();
        assert!(unnamed.validate().is_err());
        assert!(parse("[[profile]]\nname = \"x\"\nmatch = 3\n").is_err());
    }
}
