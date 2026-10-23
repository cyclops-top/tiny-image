//! Configuration precedence: built-in defaults, TOML, matching overrides, CLI.
use crate::imaging::{Format, MetadataPolicy, OptimizeOptions};
use anyhow::{Context, Result, bail};
use globset::Glob;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SettingsPatch {
    pub preset: Option<String>,
    pub score: Option<f64>,
    pub formats: Option<Vec<String>>,
    pub keep_format: Option<bool>,
    pub min_saving: Option<f64>,
    pub min_bytes: Option<u64>,
    pub include: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
    pub strip_all: Option<bool>,
    pub keep_metadata: Option<bool>,
}
impl SettingsPatch {
    pub fn overlay(&mut self, other: &Self) {
        if other.preset.is_some() && other.score.is_none() {
            self.score = None;
        }
        macro_rules! copy { ($($field:ident),*) => {$ (if other.$field.is_some() { self.$field = other.$field.clone(); })*}; }
        copy!(
            preset,
            score,
            formats,
            keep_format,
            min_saving,
            min_bytes,
            include,
            exclude,
            strip_all,
            keep_metadata
        );
        if other.strip_all == Some(true) {
            self.keep_metadata = Some(false);
        }
        if other.keep_metadata == Some(true) {
            self.strip_all = Some(false);
        }
    }
    pub fn validate(&self) -> Result<()> {
        if let Some(preset) = &self.preset
            && !["high", "balanced", "small"].contains(&preset.as_str())
        {
            bail!("unknown preset {preset:?}; choose high, balanced, small");
        }
        if let Some(score) = self.score
            && (!score.is_finite() || !(0.0..=100.0).contains(&score))
        {
            bail!("score must be a finite number between 0 and 100");
        }
        if let Some(saving) = self.min_saving
            && (!saving.is_finite() || !(0.0..=100.0).contains(&saving))
        {
            bail!("min-saving must be a finite percentage between 0 and 100");
        }
        if let Some(formats) = &self.formats {
            if formats.is_empty() {
                bail!("formats cannot be empty");
            }
            for format in formats {
                parse_format(format)?;
            }
        }
        if self.strip_all == Some(true) && self.keep_metadata == Some(true) {
            bail!("strip_all and keep_metadata are mutually exclusive");
        }
        for pattern in self.include.iter().chain(self.exclude.iter()).flatten() {
            Glob::new(pattern).with_context(|| format!("invalid glob {pattern:?}"))?;
        }
        Ok(())
    }
    pub fn optimize_options(&self, asset: bool) -> Result<OptimizeOptions> {
        self.validate()?;
        let score = self
            .score
            .unwrap_or(match self.preset.as_deref().unwrap_or("balanced") {
                "high" => 88.0,
                "small" => 70.0,
                _ => 80.0,
            });
        let formats = self
            .formats
            .clone()
            .unwrap_or_else(|| vec!["png".into(), "webp".into()])
            .iter()
            .map(|s| parse_format(s))
            .collect::<Result<Vec<_>>>()?;
        Ok(OptimizeOptions {
            formats,
            score,
            min_saving: self.min_saving.unwrap_or(10.0),
            keep_format: asset || self.keep_format.unwrap_or(false),
            metadata: if self.strip_all.unwrap_or(false) {
                MetadataPolicy::StripAll
            } else if self.keep_metadata.unwrap_or(false) {
                MetadataPolicy::Keep
            } else {
                MetadataPolicy::Default
            },
        })
    }
}
pub fn parse_format(s: &str) -> Result<Format> {
    match s.to_ascii_lowercase().as_str() {
        "png" => Ok(Format::Png),
        "webp" => Ok(Format::Webp),
        "jpeg" | "jpg" => Ok(Format::Jpeg),
        "avif" => Ok(Format::Avif),
        _ => bail!("unknown format {s:?}; choose png, webp, jpeg, avif"),
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryOverride {
    pub glob: String,
    #[serde(flatten)]
    pub settings: SettingsPatch,
}
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
    #[serde(flatten)]
    pub settings: SettingsPatch,
    #[serde(default)]
    pub overrides: Vec<DirectoryOverride>,
}
impl Config {
    pub fn read(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("read configuration {}", path.display()))?;
        let config: Self = toml::from_str(&text)
            .with_context(|| format!("parse configuration {}", path.display()))?;
        config.settings.validate()?;
        for entry in &config.overrides {
            Glob::new(&entry.glob)
                .with_context(|| format!("invalid override glob {:?}", entry.glob))?;
            entry.settings.validate()?;
            if entry.settings.include.is_some() || entry.settings.exclude.is_some() {
                bail!(
                    "include/exclude must be configured at the top level; directory overrides control encoding settings only"
                );
            }
        }
        Ok(config)
    }
    pub fn effective(&self, relative: &Path, cli: &SettingsPatch) -> Result<SettingsPatch> {
        let mut settings = self.settings.clone();
        for entry in &self.overrides {
            if Glob::new(&entry.glob)?.compile_matcher().is_match(relative) {
                settings.overlay(&entry.settings);
            }
        }
        settings.overlay(cli);
        settings.validate()?;
        Ok(settings)
    }
}
pub fn discover_config(inputs: &[PathBuf]) -> Option<PathBuf> {
    let first = inputs
        .first()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("."));
    let absolute = fs::canonicalize(&first).ok()?;
    let start = if absolute.is_file() {
        absolute.parent()?
    } else {
        absolute.as_path()
    };
    for ancestor in start.ancestors() {
        let config = ancestor.join("tinyimg.toml");
        if config.is_file() {
            return Some(config);
        }
    }
    None
}
pub const TEMPLATE: &str = r#"# tinyimg presets are experimental and have not been calibrated.
preset = "balanced"
formats = ["png", "webp"]
min_saving = 10
min_bytes = 2048
keep_format = false
# score = 88
# exclude = ["**/vendor/**"]

# Matching overrides are applied in order; CLI flags always win.
# [[overrides]]
# glob = "drawable*/**"
# score = 88
# keep_format = true
"#;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cli_wins_and_preset_resets_config_score() {
        let mut x = SettingsPatch {
            score: Some(71.),
            ..Default::default()
        };
        x.overlay(&SettingsPatch {
            preset: Some("high".into()),
            ..Default::default()
        });
        assert_eq!(x.optimize_options(false).unwrap().score, 88.);
    }
    #[test]
    fn rejects_invalid_values() {
        assert!(
            SettingsPatch {
                score: Some(f64::NAN),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            SettingsPatch {
                formats: Some(vec!["gif".into()]),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }
    #[test]
    fn parses_overrides_and_applies_cli_last() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("tinyimg.toml");
        fs::write(
            &p,
            "score=70\n[[overrides]]\nglob='drawable*/**'\nscore=90\n",
        )
        .unwrap();
        let c = Config::read(&p).unwrap();
        assert_eq!(
            c.effective(Path::new("drawable-mdpi/a.png"), &Default::default())
                .unwrap()
                .score,
            Some(90.)
        );
        assert_eq!(
            c.effective(
                Path::new("drawable-mdpi/a.png"),
                &SettingsPatch {
                    score: Some(80.),
                    ..Default::default()
                }
            )
            .unwrap()
            .score,
            Some(80.)
        );
    }
}
