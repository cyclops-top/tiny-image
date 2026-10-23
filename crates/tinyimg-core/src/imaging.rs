//! In-memory encoding, perceptual verification, bounded search and selection.
//!
//! Unsupported color management is never silently treated as sRGB. Such PNGs
//! may take the pixel-identical lossless path only; other sources are retained.
use image::{DynamicImage, GenericImageView, ImageFormat, ImageReader, RgbaImage};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    io::Cursor,
    panic::{AssertUnwindSafe, catch_unwind},
};

const MAX_DIMENSION: u32 = 8000;
// SSIMULACRA2 needs many floating-point image planes, beyond the decoder's RAM.
const MAX_PIXELS: u64 = 16_000_000;
const MAX_DECODE_BYTES: u64 = 256 * 1024 * 1024;
const SEARCH_ATTEMPTS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Png,
    Webp,
    Jpeg,
    Avif,
}
impl Format {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "png" => Ok(Self::Png),
            "webp" => Ok(Self::Webp),
            "jpeg" | "jpg" => Ok(Self::Jpeg),
            "avif" => Ok(Self::Avif),
            _ => Err(format!("unsupported format: {value}")),
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Webp => "webp",
            Self::Jpeg => "jpeg",
            Self::Avif => "avif",
        }
    }
    pub fn extension(self) -> &'static str {
        if self == Self::Jpeg {
            "jpg"
        } else {
            self.as_str()
        }
    }
    fn preference(self) -> u8 {
        match self {
            Self::Png => 0,
            Self::Webp => 1,
            Self::Jpeg => 2,
            Self::Avif => 3,
        }
    }
}
impl std::fmt::Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
impl std::str::FromStr for Format {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum MetadataPolicy {
    #[default]
    Default,
    StripAll,
    Keep,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptimizeOptions {
    pub formats: Vec<Format>,
    pub score: f64,
    /// Minimum percentage saved by conversion versus the best feasible PNG.
    pub min_saving: f64,
    pub keep_format: bool,
    pub metadata: MetadataPolicy,
}
impl Default for OptimizeOptions {
    fn default() -> Self {
        Self {
            formats: vec![Format::Png, Format::Webp],
            score: 80.0,
            min_saving: 10.0,
            keep_format: false,
            metadata: MetadataPolicy::Default,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateSummary {
    pub encoder: String,
    pub format: Format,
    pub quality: Option<u8>,
    pub size: usize,
    pub score: Option<f64>,
    pub lossless: bool,
    /// True when the decoded candidate meets the quality/alpha requirements.
    /// Selection additionally requires a smaller file and the conversion gate.
    pub accepted: bool,
    pub note: Option<String>,
}
#[derive(Debug, Clone)]
pub struct Candidate {
    pub bytes: Vec<u8>,
    pub summary: CandidateSummary,
}
#[derive(Debug, Clone)]
pub struct Optimization {
    pub source_format: Format,
    pub width: u32,
    pub height: u32,
    pub has_alpha: bool,
    pub class: String,
    pub selected: Option<Candidate>,
    pub candidates: Vec<CandidateSummary>,
    pub warnings: Vec<String>,
    pub skip_reason: Option<String>,
    /// All attempted encoders/verifiers failed, rather than merely missing the
    /// requested score. The caller should report a runtime-error exit status.
    pub runtime_error: Option<String>,
}

#[derive(Debug, Default)]
struct Restrictions {
    lossless_only: Option<String>,
    unmanaged_color: bool,
    skip: Option<String>,
}

/// Optimize one complete file without writing anything. A returned candidate has
/// been decoded and verified twice, including a final independent verification.
pub fn optimize_bytes(input: &[u8], options: &OptimizeOptions) -> Result<Optimization, String> {
    if !options.score.is_finite() || !(0.0..=100.0).contains(&options.score) {
        return Err("score must be finite and between 0 and 100".into());
    }
    if !options.min_saving.is_finite() || !(0.0..=100.0).contains(&options.min_saving) {
        return Err("min-saving must be finite and between 0 and 100 percent".into());
    }
    let detected = image::guess_format(input).map_err(|e| format!("unrecognized image: {e}"))?;
    let source_format = match detected {
        ImageFormat::Png => Format::Png,
        ImageFormat::WebP => Format::Webp,
        ImageFormat::Jpeg => Format::Jpeg,
        _ => {
            return Err(
                "unsupported input format (only static PNG, WebP and JPEG are decoded)".into(),
            );
        }
    };
    let restrictions = inspect_container(input, source_format)?;
    let (width, height) = image_dimensions(input, detected)?;
    let mut result = Optimization {
        source_format,
        width,
        height,
        has_alpha: false,
        class: "unknown".into(),
        selected: None,
        candidates: Vec::new(),
        warnings: Vec::new(),
        skip_reason: None,
        runtime_error: None,
    };
    if width == 0
        || height == 0
        || width > MAX_DIMENSION
        || height > MAX_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_PIXELS
    {
        result.skip_reason = Some(format!(
            "image exceeds safety limits: maximum side {MAX_DIMENSION}, maximum pixels {MAX_PIXELS}"
        ));
        return Ok(result);
    }
    if let Some(reason) = restrictions.skip {
        result.skip_reason = Some(reason);
        return Ok(result);
    }
    if restrictions.unmanaged_color && options.metadata == MetadataPolicy::StripAll {
        result.skip_reason = Some("strip-all would remove an unmanaged color profile/gamma and change displayed colors; original retained".into());
        return Ok(result);
    }
    let decoded = decode(input, detected)?;
    let rgba = decoded.to_rgba8();
    result.has_alpha = has_transparency(&decoded);
    result.class = classify(&rgba);
    let tiny = width < 8 || height < 8;
    let mut limited = restrictions.lossless_only;
    if tiny {
        result.warnings.push("SSIMULACRA2 requires at least 8x8 pixels; only pixel-identical lossless candidates are eligible".into());
    }
    if options.metadata == MetadataPolicy::Keep {
        limited = Some("keep-metadata currently supports only PNG lossless optimization; conversion and quantization are disabled".into());
    }
    if let Some(reason) = &limited {
        result.warnings.push(reason.clone());
    }
    if options.formats.contains(&Format::Avif) {
        result.warnings.push(
            "AVIF is explicitly unsupported in this build: no verified AVIF encode/decode path"
                .into(),
        );
    }
    if options.metadata == MetadataPolicy::Keep && source_format != Format::Png {
        result.skip_reason = Some(
            "keep-metadata is not implemented for this source format; original retained".into(),
        );
        return Ok(result);
    }
    if limited.is_some() && source_format != Format::Png {
        result.skip_reason =
            Some("source needs unsupported color/metadata handling; original retained".into());
        return Ok(result);
    }
    let allowed = |format| {
        options.formats.contains(&format) && (!options.keep_format || format == source_format)
    };
    let lossy_allowed = limited.is_none() && !tiny;
    let cross_lossless_allowed = limited.is_none();
    let source = Source {
        decoded: &decoded,
        rgba: &rgba,
        has_alpha: result.has_alpha,
        tiny,
        target: options.score,
    };
    let mut feasible = Vec::new();
    if allowed(Format::Png) {
        if source_format == Format::Png {
            let candidate = encode_oxipng(input, options.metadata);
            record_candidate(
                candidate,
                &source,
                Codec::PngLossless,
                None,
                &mut result,
                &mut feasible,
            );
        } else if cross_lossless_allowed {
            let candidate = encode_png(&rgba).and_then(|b| encode_oxipng(&b, options.metadata));
            record_candidate(
                candidate,
                &source,
                Codec::PngLossless,
                None,
                &mut result,
                &mut feasible,
            );
        }
        if lossy_allowed {
            search_codec(Codec::PngQuant, &source, &mut result, &mut feasible);
        }
    }
    if allowed(Format::Webp) && cross_lossless_allowed {
        record_candidate(
            encode_codec(Codec::WebpLossless, &rgba, None),
            &source,
            Codec::WebpLossless,
            None,
            &mut result,
            &mut feasible,
        );
        if lossy_allowed {
            search_codec(Codec::WebpLossy, &source, &mut result, &mut feasible);
        }
    }
    if allowed(Format::Jpeg) {
        if result.has_alpha {
            result
                .warnings
                .push("JPEG skipped: source has non-opaque pixels".into());
        } else if lossy_allowed {
            search_codec(Codec::Jpeg, &source, &mut result, &mut feasible);
        }
    }
    result.runtime_error = all_candidates_failed(&result.candidates);
    let selected = select_candidate(&feasible, source_format, input.len(), options.min_saving);
    if let Some(index) = selected {
        let candidate = feasible.swap_remove(index);
        // Never trust a search-stage score in place of final-byte verification.
        match verify(&source, &candidate.bytes, candidate.summary.lossless) {
            Ok(score)
                if candidate.bytes.len() < input.len()
                    && score.is_none_or(|s| s >= options.score) =>
            {
                let mut candidate = candidate;
                candidate.summary.score = score;
                result.selected = Some(candidate);
            }
            Ok(_) => result
                .warnings
                .push("final quality/size verification failed; original retained".into()),
            Err(error) => result.warnings.push(format!(
                "final verification failed; original retained: {error}"
            )),
        }
    }
    if result.selected.is_none() {
        result.skip_reason = Some(
            if feasible.is_empty() {
                "no eligible candidate meets the quality requirements"
            } else {
                "no candidate improves size while satisfying the conversion threshold"
            }
            .into(),
        );
    }
    Ok(result)
}

fn image_dimensions(input: &[u8], format: ImageFormat) -> Result<(u32, u32), String> {
    ImageReader::with_format(Cursor::new(input), format)
        .into_dimensions()
        .map_err(|e| format!("invalid image header: {e}"))
}
fn decode(input: &[u8], format: ImageFormat) -> Result<DynamicImage, String> {
    let mut reader = ImageReader::with_format(Cursor::new(input), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(limits);
    reader.decode().map_err(|e| format!("decode failed: {e}"))
}

fn has_transparency(image: &DynamicImage) -> bool {
    match image {
        DynamicImage::ImageRgba8(pixels) => pixels.pixels().any(|p| p[3] != u8::MAX),
        DynamicImage::ImageLumaA8(pixels) => pixels.pixels().any(|p| p[1] != u8::MAX),
        DynamicImage::ImageRgba16(pixels) => pixels.pixels().any(|p| p[3] != u16::MAX),
        DynamicImage::ImageLumaA16(pixels) => pixels.pixels().any(|p| p[1] != u16::MAX),
        DynamicImage::ImageRgba32F(pixels) => pixels.pixels().any(|p| p[3] != 1.0),
        _ => false,
    }
}

fn inspect_container(input: &[u8], format: Format) -> Result<Restrictions, String> {
    let mut r = Restrictions::default();
    match format {
        Format::Png => {
            if input.len() < 33 {
                return Err("truncated PNG".into());
            }
            if input[24] == 16 {
                r.lossless_only = Some("16-bit PNG restricted to exact lossless optimization; no 8-bit down-conversion".into());
            }
            let chunks = png_chunks(input)?;
            let explicit_srgb = chunks
                .iter()
                .any(|(name, data)| name == b"sRGB" && data.len() == 1 && data[0] <= 3);
            for (name, data) in chunks {
                match &name {
                    b"acTL" => r.skip = Some("animated PNG is not supported".into()),
                    b"eXIf" => r.skip = Some("PNG EXIF orientation is not normalized; original retained to avoid rotation changes".into()),
                    b"iCCP" | b"cHRM" | b"cICP" | b"mDCv" | b"cLLi" => {
                        r.unmanaged_color=true;
                        r.lossless_only=Some("PNG has color-management metadata; only lossless optimization is safe without a verified color transform".into());
                    },
                    b"gAMA" if !explicit_srgb || data != 45455u32.to_be_bytes() => {
                        r.unmanaged_color=true;
                        r.lossless_only=Some("non-sRGB PNG gamma requires color management; only lossless optimization is allowed".into());
                    },
                    b"caBX" => r.skip = Some("signed C2PA PNG retained; optimization would invalidate provenance".into()),
                    _ => (),
                }
            }
        }
        Format::Webp => {
            if input.len() < 12 {
                return Err("truncated WebP".into());
            }
            let mut p = 12usize;
            while p < input.len() {
                if p + 8 > input.len() {
                    return Err("truncated WebP chunk".into());
                }
                let name = &input[p..p + 4];
                let len = u32::from_le_bytes(input[p + 4..p + 8].try_into().unwrap()) as usize;
                let end = p
                    .checked_add(8)
                    .and_then(|v| v.checked_add(len))
                    .ok_or("WebP chunk length overflow")?;
                if end > input.len() {
                    return Err("truncated WebP chunk data".into());
                }
                if name == b"ANIM"
                    || name == b"ANMF"
                    || (name == b"VP8X" && len > 0 && input[p + 8] & 2 != 0)
                {
                    r.skip = Some("animated WebP is not supported".into());
                }
                if name == b"ICCP" || name == b"EXIF" {
                    r.skip =
                        Some("WebP ICC/EXIF handling is not implemented; original retained".into());
                }
                p = end
                    .checked_add(len & 1)
                    .ok_or("WebP chunk padding overflow")?;
            }
        }
        Format::Jpeg => {
            let mut p = 2usize;
            while p < input.len() {
                if input[p] != 0xff {
                    return Err("invalid JPEG marker sequence".into());
                }
                while p < input.len() && input[p] == 0xff {
                    p += 1;
                }
                if p >= input.len() {
                    return Err("truncated JPEG marker".into());
                }
                let marker = input[p];
                p += 1;
                if marker == 0xda || marker == 0xd9 {
                    break;
                }
                if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
                    continue;
                }
                if p + 2 > input.len() {
                    return Err("truncated JPEG segment".into());
                }
                let len = u16::from_be_bytes(input[p..p + 2].try_into().unwrap()) as usize;
                if len < 2 || p.checked_add(len).is_none_or(|end| end > input.len()) {
                    return Err("invalid JPEG segment length".into());
                }
                let data = &input[p + 2..p + len];
                if (marker == 0xe1 && data.starts_with(b"Exif\0\0"))
                    || (marker == 0xe2 && data.starts_with(b"ICC_PROFILE\0"))
                {
                    r.skip = Some("JPEG EXIF/ICC handling is not implemented; original retained to preserve orientation and color".into());
                }
                if (0xc0..=0xcf).contains(&marker) && ![0xc4, 0xc8, 0xcc].contains(&marker) {
                    if data.len() < 6 {
                        return Err("truncated JPEG frame".into());
                    }
                    if data[0] != 8 || ![1, 3].contains(&data[5]) {
                        r.skip = Some(
                            "unsupported JPEG precision or CMYK/YCCK color; original retained"
                                .into(),
                        );
                    }
                }
                p += len;
            }
        }
        Format::Avif => r.skip = Some("AVIF input is not supported".into()),
    }
    Ok(r)
}

type PngChunk<'a> = ([u8; 4], &'a [u8]);
fn png_chunks(input: &[u8]) -> Result<Vec<PngChunk<'_>>, String> {
    let mut chunks = Vec::new();
    let mut p = 8usize;
    let mut ended = false;
    while p < input.len() {
        if p + 12 > input.len() {
            return Err("truncated PNG chunk".into());
        }
        let len = u32::from_be_bytes(input[p..p + 4].try_into().unwrap()) as usize;
        let end = p
            .checked_add(12)
            .and_then(|v| v.checked_add(len))
            .ok_or("PNG chunk length overflow")?;
        if end > input.len() {
            return Err("truncated PNG chunk payload".into());
        }
        let name: [u8; 4] = input[p + 4..p + 8].try_into().unwrap();
        chunks.push((name, &input[p + 8..end - 4]));
        p = end;
        if &name == b"IEND" {
            ended = true;
            break;
        }
    }
    if !ended {
        return Err("PNG has no IEND".into());
    }
    Ok(chunks)
}

fn classify(image: &RgbaImage) -> String {
    let mut colors = HashSet::new();
    let mut edges = 0u64;
    let mut pairs = 0u64;
    for (x, y, p) in image.enumerate_pixels() {
        if colors.len() <= 512 {
            colors.insert(if p[3] == 0 { [0; 4] } else { p.0 });
        }
        if x > 0 {
            let q = image.get_pixel(x - 1, y);
            if p.0
                .iter()
                .zip(q.0)
                .map(|(a, b)| (*a as i32 - b as i32).unsigned_abs())
                .sum::<u32>()
                > 192
            {
                edges += 1;
            }
            pairs += 1;
        }
    }
    if colors.len() <= 256 || (pairs > 0 && edges as f64 / pairs as f64 > 0.20) {
        "ui/icon"
    } else {
        "photo/gradient"
    }
    .into()
}

#[derive(Clone, Copy, Debug)]
enum Codec {
    PngLossless,
    PngQuant,
    WebpLossless,
    WebpLossy,
    Jpeg,
}
impl Codec {
    fn name(self) -> &'static str {
        match self {
            Self::PngLossless => "oxipng",
            Self::PngQuant => "imagequant+oxipng",
            Self::WebpLossless => "webp-lossless",
            Self::WebpLossy => "webp-lossy",
            Self::Jpeg => "mozjpeg",
        }
    }
    fn format(self) -> Format {
        match self {
            Self::PngLossless | Self::PngQuant => Format::Png,
            Self::WebpLossless | Self::WebpLossy => Format::Webp,
            Self::Jpeg => Format::Jpeg,
        }
    }
    fn lossless(self) -> bool {
        matches!(self, Self::PngLossless | Self::WebpLossless)
    }
}
struct Source<'a> {
    decoded: &'a DynamicImage,
    rgba: &'a RgbaImage,
    has_alpha: bool,
    tiny: bool,
    target: f64,
}

fn encode_png(rgba: &RgbaImage) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, rgba.width(), rgba.height());
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer
            .write_image_data(rgba.as_raw())
            .map_err(|e| e.to_string())?;
        writer.finish().map_err(|e| e.to_string())?;
    }
    Ok(bytes)
}
fn encode_oxipng(input: &[u8], metadata: MetadataPolicy) -> Result<Vec<u8>, String> {
    let mut options = oxipng::Options::from_preset(2);
    options.optimize_alpha = true;
    options.max_decompressed_size = Some(MAX_DECODE_BYTES as usize);
    options.strip = match metadata {
        MetadataPolicy::Keep => oxipng::StripChunks::None,
        MetadataPolicy::StripAll => oxipng::StripChunks::All,
        MetadataPolicy::Default => oxipng::StripChunks::Strip([*b"eXIf"].into_iter().collect()),
    };
    // Do not remove profiles or associated colorspace chunks indirectly through
    // grayscale/type reductions. Pixel-identity alone cannot prove ICC semantics.
    if metadata != MetadataPolicy::StripAll
        && png_chunks(input)?
            .iter()
            .any(|(n, _)| [*b"iCCP", *b"cHRM", *b"gAMA", *b"cICP"].contains(n))
    {
        options.color_type_reduction = false;
        options.grayscale_reduction = false;
    }
    if metadata == MetadataPolicy::Keep {
        options.color_type_reduction = false;
        options.grayscale_reduction = false;
        options.bit_depth_reduction = false;
        options.palette_reduction = false;
    }
    let encoded = oxipng::optimize_from_memory(input, &options).map_err(|e| e.to_string())?;
    if metadata != MetadataPolicy::StripAll {
        let before = png_chunks(input)?;
        let after = png_chunks(&encoded)?;
        for (name, data) in before {
            let must_keep = if metadata == MetadataPolicy::Keep {
                name[0].is_ascii_lowercase()
            } else {
                [
                    *b"iCCP", *b"cHRM", *b"gAMA", *b"sRGB", *b"cICP", *b"mDCv", *b"cLLi",
                ]
                .contains(&name)
            };
            if must_keep && !after.iter().any(|(n, d)| *n == name && *d == data) {
                return Err(format!(
                    "lossless optimization would discard protected {} metadata",
                    String::from_utf8_lossy(&name)
                ));
            }
        }
    }
    Ok(encoded)
}
fn encode_codec(codec: Codec, rgba: &RgbaImage, quality: Option<u8>) -> Result<Vec<u8>, String> {
    let q = quality.unwrap_or(100);
    match codec {
        Codec::PngLossless => {
            encode_png(rgba).and_then(|b| encode_oxipng(&b, MetadataPolicy::Default))
        }
        Codec::PngQuant => {
            let mut attributes = imagequant::new();
            attributes.set_speed(4).map_err(|e| e.to_string())?;
            attributes.set_quality(0, q).map_err(|e| e.to_string())?;
            let pixels: Vec<_> = rgba
                .pixels()
                .map(|p| imagequant::RGBA::new(p[0], p[1], p[2], p[3]))
                .collect();
            let mut image = attributes
                .new_image(pixels, rgba.width() as usize, rgba.height() as usize, 0.0)
                .map_err(|e| e.to_string())?;
            let mut quantized = attributes.quantize(&mut image).map_err(|e| e.to_string())?;
            quantized
                .set_dithering_level(1.0)
                .map_err(|e| e.to_string())?;
            let (palette, indices) = quantized.remapped(&mut image).map_err(|e| e.to_string())?;
            let mut bytes = Vec::new();
            {
                let mut encoder = png::Encoder::new(&mut bytes, rgba.width(), rgba.height());
                encoder.set_color(png::ColorType::Indexed);
                encoder.set_depth(png::BitDepth::Eight);
                encoder.set_palette(
                    palette
                        .iter()
                        .flat_map(|p| [p.r, p.g, p.b])
                        .collect::<Vec<_>>(),
                );
                encoder.set_trns(palette.iter().map(|p| p.a).collect::<Vec<_>>());
                let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
                writer
                    .write_image_data(&indices)
                    .map_err(|e| e.to_string())?;
                writer.finish().map_err(|e| e.to_string())?;
            }
            encode_oxipng(&bytes, MetadataPolicy::Default)
        }
        Codec::WebpLossless | Codec::WebpLossy => {
            let encoder = webp::Encoder::from_rgba(rgba.as_raw(), rgba.width(), rgba.height());
            let mut config =
                webp::WebPConfig::new().map_err(|_| "cannot initialize WebP encoder")?;
            config.lossless = i32::from(codec.lossless());
            config.quality = if codec.lossless() {
                100.0
            } else {
                f32::from(q)
            };
            config.method = 6;
            config.alpha_quality = 100;
            config.exact = 1;
            encoder
                .encode_advanced(&config)
                .map(|b| b.to_vec())
                .map_err(|e| format!("WebP encoding failed: {e:?}"))
        }
        Codec::Jpeg => {
            // MozJPEG signals some codec errors by unwinding, per its API.
            catch_unwind(AssertUnwindSafe(|| -> Result<Vec<u8>, String> {
                let mut encoder = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_RGB);
                encoder.set_size(rgba.width() as usize, rgba.height() as usize);
                encoder.set_quality(f32::from(q));
                encoder.set_progressive_mode();
                let mut encoder = encoder
                    .start_compress(Vec::new())
                    .map_err(|e| e.to_string())?;
                let rgb: Vec<u8> = rgba.pixels().flat_map(|p| [p[0], p[1], p[2]]).collect();
                encoder.write_scanlines(&rgb).map_err(|e| e.to_string())?;
                encoder.finish().map_err(|e| e.to_string())
            }))
            .map_err(|_| "MozJPEG encoder panicked".to_string())?
        }
    }
}

fn record_candidate(
    encoded: Result<Vec<u8>, String>,
    source: &Source<'_>,
    codec: Codec,
    quality: Option<u8>,
    result: &mut Optimization,
    feasible: &mut Vec<Candidate>,
) -> Option<f64> {
    let mut summary = CandidateSummary {
        encoder: codec.name().into(),
        format: codec.format(),
        quality,
        size: 0,
        score: None,
        lossless: codec.lossless(),
        accepted: false,
        note: None,
    };
    let mut score = None;
    match encoded {
        Ok(bytes) => {
            summary.size = bytes.len();
            match verify(source, &bytes, codec.lossless()) {
                Ok(s) => {
                    summary.score = s;
                    summary.accepted = s.is_none_or(|score| score >= source.target);
                    score = Some(s.unwrap_or(100.0));
                    if summary.accepted {
                        if codec.lossless() {
                            summary.note = Some(if source.tiny {
                                "exact decoded-pixel proof; metric unavailable below 8x8"
                            } else {
                                "exact decoded-pixel proof; score 100 inferred from identity, not measured"
                            }.into());
                        }
                        feasible.push(Candidate {
                            bytes,
                            summary: summary.clone(),
                        });
                    } else {
                        summary.note = Some(format!(
                            "score {:.4} is below target {:.4}",
                            s.unwrap(),
                            source.target
                        ));
                    }
                }
                Err(error) => summary.note = Some(error),
            }
        }
        Err(error) => summary.note = Some(error),
    }
    result.candidates.push(summary);
    score
}
fn search_codec(
    codec: Codec,
    source: &Source<'_>,
    result: &mut Optimization,
    feasible: &mut Vec<Candidate>,
) {
    quality_search(source.target, |q| {
        record_candidate(
            encode_codec(codec, source.rgba, Some(q)),
            source,
            codec,
            Some(q),
            result,
            feasible,
        )
    });
}

fn all_candidates_failed(candidates: &[CandidateSummary]) -> Option<String> {
    if candidates.is_empty() || candidates.iter().any(|c| c.accepted || c.score.is_some()) {
        return None;
    }
    let mut reasons: Vec<_> = candidates
        .iter()
        .filter_map(|c| c.note.as_ref().map(|note| format!("{}: {note}", c.encoder)))
        .collect();
    reasons.sort();
    reasons.dedup();
    Some(format!(
        "all attempted encoders/verifiers failed: {}",
        reasons.join("; ")
    ))
}

/// Bounded secant search with bisection fallback. Every evaluated candidate is
/// retained by the caller, including non-monotonic feasible points. This is a
/// finite search, not a claim of a globally optimal codec quality parameter.
fn quality_search(target: f64, mut evaluate: impl FnMut(u8) -> Option<f64>) -> Vec<u8> {
    let mut tested = Vec::new();
    let mut observations: Vec<(u8, f64)> = Vec::new();
    for _ in 0..SEARCH_ATTEMPTS {
        let q = if tested.is_empty() {
            1
        } else if tested.len() == 1 {
            100
        } else {
            let pass = observations
                .iter()
                .filter(|(_, s)| *s >= target)
                .min_by_key(|(q, _)| *q)
                .copied();
            let fail = observations
                .iter()
                .filter(|(q, s)| *s < target && pass.is_none_or(|(p, _)| *q < p))
                .max_by_key(|(q, _)| *q)
                .copied();
            match (fail, pass) {
                (_, Some((1, _))) => break,
                (Some((lo, sl)), Some((hi, sh))) if hi > lo + 1 => {
                    let mid = lo + (hi - lo) / 2;
                    let secant = if sh > sl {
                        (f64::from(lo) + (target - sl) * f64::from(hi - lo) / (sh - sl)).round()
                            as u8
                    } else {
                        mid
                    };
                    // Alternate bisection to bound progress under flat/noisy scores.
                    if tested.len() % 2 == 1 || secant <= lo || secant >= hi {
                        mid
                    } else {
                        secant
                    }
                }
                (Some(_), Some(_)) => break,
                _ => {
                    // Missing bracket, failed encodes or high-quality non-monotonicity:
                    // probe the largest untested interval rather than discard it.
                    let mut sorted = tested.clone();
                    sorted.sort_unstable();
                    let Some(pair) = sorted
                        .windows(2)
                        .filter(|p| p[1] > p[0] + 1)
                        .max_by_key(|p| p[1] - p[0])
                    else {
                        break;
                    };
                    pair[0] + (pair[1] - pair[0]) / 2
                }
            }
        };
        if tested.contains(&q) {
            break;
        }
        tested.push(q);
        if let Some(score) = evaluate(q).filter(|s| s.is_finite()) {
            observations.push((q, score));
        }
    }
    tested
}

fn verify(source: &Source<'_>, bytes: &[u8], lossless: bool) -> Result<Option<f64>, String> {
    let format = image::guess_format(bytes).map_err(|e| e.to_string())?;
    let candidate = decode(bytes, format)?;
    if candidate.dimensions() != source.decoded.dimensions() {
        return Err("candidate dimensions changed".into());
    }
    let rgba = candidate.to_rgba8();
    if source.has_alpha && !has_transparency(&candidate) {
        return Err("candidate flattened source transparency".into());
    }
    if lossless {
        if !visually_exact(source.decoded, &candidate) {
            return Err("lossless encoder changed visible decoded pixels".into());
        }
        return Ok(if source.tiny { None } else { Some(100.0) });
    }
    if source.tiny {
        return Err("lossy scoring unavailable below 8x8".into());
    }
    let score = perceptual_score(source.rgba, &rgba)?;
    Ok(Some(score))
}
fn visually_exact(a: &DynamicImage, b: &DynamicImage) -> bool {
    if a.dimensions() != b.dimensions() {
        return false;
    }
    let a = a.to_rgba16();
    let b = b.to_rgba16();
    a.pixels()
        .zip(b.pixels())
        .all(|(p, q)| p[3] == q[3] && (p[3] == 0 || p == q))
}

/// Compare sRGB pixels in linear-light composites, separately over black and
/// white. Taking the lower score catches both bright and dark alpha artifacts.
pub fn perceptual_score(reference: &RgbaImage, candidate: &RgbaImage) -> Result<f64, String> {
    if reference.dimensions() != candidate.dimensions() {
        return Err("metric dimensions differ".into());
    }
    if reference.width() < 8 || reference.height() < 8 {
        return Err("SSIMULACRA2 requires at least 8x8 pixels".into());
    }
    let transparent = reference
        .pixels()
        .chain(candidate.pixels())
        .any(|p| p[3] != 255);
    let black = score_background(reference, candidate, 0.0)?;
    let score = if transparent {
        black.min(score_background(reference, candidate, 1.0)?)
    } else {
        black
    };
    if score.is_finite() {
        Ok(score.min(100.0))
    } else {
        Err("metric returned a non-finite score".into())
    }
}
fn srgb_to_linear(v: u8) -> f32 {
    let s = f32::from(v) / 255.0;
    if s <= 0.04045 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}
fn composite(image: &RgbaImage, background: f32) -> Vec<[f32; 3]> {
    image
        .pixels()
        .map(|p| {
            let alpha = f32::from(p[3]) / 255.0;
            [0, 1, 2].map(|c| srgb_to_linear(p[c]) * alpha + background * (1.0 - alpha))
        })
        .collect()
}
fn score_background(
    reference: &RgbaImage,
    candidate: &RgbaImage,
    background: f32,
) -> Result<f64, String> {
    let w = reference.width() as usize;
    let h = reference.height() as usize;
    let a = ssimulacra2::LinearRgb::new(composite(reference, background), w, h)
        .map_err(|e| format!("metric input: {e}"))?;
    let b = ssimulacra2::LinearRgb::new(composite(candidate, background), w, h)
        .map_err(|e| format!("metric input: {e}"))?;
    ssimulacra2::compute_frame_ssimulacra2(a, b).map_err(|e| e.to_string())
}

fn select_candidate(
    candidates: &[Candidate],
    source: Format,
    original_size: usize,
    min_saving: f64,
) -> Option<usize> {
    let png_baseline = candidates
        .iter()
        .filter(|c| c.summary.accepted && c.summary.format == Format::Png)
        .map(|c| c.bytes.len())
        .chain((source == Format::Png).then_some(original_size))
        .min();
    let eligible = |c: &Candidate| {
        if !c.summary.accepted || c.bytes.len() >= original_size {
            return false;
        }
        if source == Format::Png
            && c.summary.format != Format::Png
            && let Some(png_size) = png_baseline
        {
            return (png_size.saturating_sub(c.bytes.len()) as f64) * 100.0 + 1e-9
                >= png_size as f64 * min_saving;
        }
        true
    };
    let smallest = candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| eligible(c))
        .min_by_key(|(_, c)| c.bytes.len())?;
    let minimum = smallest.1.bytes.len();
    // Only a 0.5% near tie may favor the simpler format; never bypass the gate.
    candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| eligible(c) && c.bytes.len() as f64 <= minimum as f64 * 1.005)
        .min_by_key(|(_, c)| (c.summary.format.preference(), c.bytes.len()))
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgba};

    fn options(formats: Vec<Format>) -> OptimizeOptions {
        OptimizeOptions {
            formats,
            ..OptimizeOptions::default()
        }
    }
    fn fixture(w: u32, h: u32, alpha: bool) -> RgbaImage {
        ImageBuffer::from_fn(w, h, |x, y| {
            Rgba([
                ((x * 13 + y * 3) % 256) as u8,
                ((x * 5 + y * 11) % 256) as u8,
                ((x ^ y) * 7 % 256) as u8,
                if alpha {
                    ((x * 7 + y * 17) % 256) as u8
                } else {
                    255
                },
            ])
        })
    }
    fn candidate(format: Format, size: usize) -> Candidate {
        Candidate {
            bytes: vec![0; size],
            summary: CandidateSummary {
                encoder: "test".into(),
                format,
                quality: Some(75),
                size,
                score: Some(90.0),
                lossless: false,
                accepted: true,
                note: None,
            },
        }
    }
    fn png_with_chunk(rgba: &RgbaImage, name: [u8; 4], data: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, rgba.width(), rgba.height());
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer
                .write_chunk(png::chunk::ChunkType(name), data)
                .unwrap();
            writer.write_image_data(rgba.as_raw()).unwrap();
            writer.finish().unwrap();
        }
        bytes
    }
    #[test]
    fn identical_ssimulacra2_is_100() {
        let image = fixture(16, 16, false);
        assert!((perceptual_score(&image, &image).unwrap() - 100.0).abs() < 1e-8);
    }
    #[test]
    fn invisible_rgb_is_ignored() {
        let a = RgbaImage::from_pixel(16, 16, Rgba([255, 0, 60, 0]));
        let b = RgbaImage::from_pixel(16, 16, Rgba([0, 255, 255, 0]));
        assert_eq!(perceptual_score(&a, &b).unwrap(), 100.0);
        assert!(visually_exact(
            &DynamicImage::ImageRgba8(a),
            &DynamicImage::ImageRgba8(b)
        ));
    }
    #[test]
    fn transparent_score_uses_worse_background() {
        let a = RgbaImage::from_pixel(16, 16, Rgba([0, 0, 0, 80]));
        let b = RgbaImage::from_pixel(16, 16, Rgba([0, 0, 0, 160]));
        let black = score_background(&a, &b, 0.0).unwrap();
        let white = score_background(&a, &b, 1.0).unwrap();
        assert_eq!(black, 100.0);
        assert!(
            white < 100.0,
            "white score must detect the alpha change: {white}"
        );
        assert!((perceptual_score(&a, &b).unwrap() - black.min(white)).abs() < 1e-8);
    }
    #[test]
    fn compositing_is_linear_light() {
        let image = RgbaImage::from_pixel(1, 1, Rgba([255, 0, 0, 128]));
        let linear = composite(&image, 0.0)[0];
        assert!((linear[0] - 128.0 / 255.0).abs() < 1e-6);
        assert_eq!(linear[1], 0.0);
    }
    #[test]
    fn metric_rejects_tiny_and_mismatched_images() {
        assert!(perceptual_score(&fixture(7, 8, false), &fixture(7, 8, false)).is_err());
        assert!(perceptual_score(&fixture(8, 8, false), &fixture(9, 8, false)).is_err());
    }
    #[test]
    fn search_brackets_monotonic_quality_with_bounded_evaluations() {
        let mut feasible = Vec::new();
        let visited = quality_search(80.0, |q| {
            let score = f64::from(q);
            if score >= 80.0 {
                feasible.push(q);
            }
            Some(score)
        });
        assert!(visited.len() <= SEARCH_ATTEMPTS);
        assert_eq!(visited.iter().collect::<HashSet<_>>().len(), visited.len());
        assert_eq!(feasible.into_iter().min(), Some(80));
    }
    #[test]
    fn search_bisects_when_scores_flat_or_encodes_fail() {
        assert_eq!(quality_search(80.0, |_| Some(40.0)).len(), SEARCH_ATTEMPTS);
        assert_eq!(quality_search(80.0, |_| None).len(), SEARCH_ATTEMPTS);
        assert_eq!(quality_search(80.0, |_| Some(100.0)), vec![1, 100]);
    }
    #[test]
    fn search_retains_all_nonmonotonic_feasible_probes() {
        let mut feasible = Vec::new();
        quality_search(80.0, |q| {
            let s = if q == 100 {
                75.0
            } else if q >= 50 {
                90.0
            } else {
                20.0
            };
            if s >= 80.0 {
                feasible.push(q);
            }
            Some(s)
        });
        assert!(!feasible.is_empty());
        assert!(feasible.contains(&50));
    }
    #[test]
    fn conversion_gate_uses_best_png_not_original() {
        let candidates = vec![candidate(Format::Png, 500), candidate(Format::Webp, 460)];
        assert_eq!(
            select_candidate(&candidates, Format::Png, 1000, 10.0),
            Some(0)
        );
        let candidates = vec![candidate(Format::Png, 500), candidate(Format::Webp, 450)];
        assert_eq!(
            select_candidate(&candidates, Format::Png, 1000, 10.0),
            Some(1)
        );
    }
    #[test]
    fn conversion_gate_applies_when_png_not_requested() {
        assert_eq!(
            select_candidate(&[candidate(Format::Webp, 950)], Format::Png, 1000, 10.0),
            None
        );
        assert_eq!(
            select_candidate(&[candidate(Format::Webp, 900)], Format::Png, 1000, 10.0),
            Some(0)
        );
    }
    #[test]
    fn near_tie_favors_png_and_never_selects_larger_than_original() {
        let cs = vec![candidate(Format::Webp, 1000), candidate(Format::Png, 1004)];
        assert_eq!(select_candidate(&cs, Format::Png, 2000, 0.0), Some(1));
        assert_eq!(
            select_candidate(&[candidate(Format::Png, 100)], Format::Png, 100, 0.0),
            None
        );
    }
    #[test]
    fn all_real_encoders_round_trip_and_lossless_preserves_alpha() {
        let image = fixture(32, 32, true);
        let source_image = DynamicImage::ImageRgba8(image.clone());
        let source = Source {
            decoded: &source_image,
            rgba: &image,
            has_alpha: true,
            tiny: false,
            target: 0.0,
        };
        for codec in [
            Codec::PngLossless,
            Codec::PngQuant,
            Codec::WebpLossless,
            Codec::WebpLossy,
        ] {
            let bytes = encode_codec(codec, &image, Some(90)).unwrap();
            assert!(
                verify(&source, &bytes, codec.lossless())
                    .unwrap()
                    .unwrap()
                    .is_finite()
            );
            let decoded = image::load_from_memory(&bytes).unwrap();
            assert_eq!(decoded.dimensions(), (32, 32));
            assert!(decoded.to_rgba8().pixels().any(|p| p[3] < 255));
            if codec.lossless() {
                assert!(visually_exact(&source_image, &decoded));
            }
        }
        let opaque = fixture(32, 32, false);
        let jpeg = encode_codec(Codec::Jpeg, &opaque, Some(90)).unwrap();
        assert_eq!(image::guess_format(&jpeg).unwrap(), ImageFormat::Jpeg);
        assert_eq!(
            image::load_from_memory(&jpeg).unwrap().dimensions(),
            (32, 32)
        );
    }
    #[test]
    fn tiny_sources_only_get_lossless_candidates() {
        let bytes = encode_png(&fixture(4, 4, true)).unwrap();
        let result = optimize_bytes(
            &bytes,
            &options(vec![Format::Png, Format::Webp, Format::Jpeg]),
        )
        .unwrap();
        assert!(!result.candidates.is_empty());
        assert!(result.candidates.iter().all(|c| c.lossless));
        assert!(result.candidates.iter().all(|c| c.score.is_none()));
    }
    #[test]
    fn keep_metadata_disables_quantization_and_conversion() {
        let bytes = png_with_chunk(&fixture(16, 16, false), *b"tEXt", b"Note\0retain me");
        let mut opts = options(vec![Format::Png, Format::Webp]);
        opts.metadata = MetadataPolicy::Keep;
        let result = optimize_bytes(&bytes, &opts).unwrap();
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(result.candidates[0].encoder, "oxipng");
        let encoded = encode_oxipng(&bytes, MetadataPolicy::Keep).unwrap();
        assert!(
            png_chunks(&encoded)
                .unwrap()
                .iter()
                .any(|(n, d)| n == b"tEXt" && *d == b"Note\0retain me")
        );
    }
    #[test]
    fn non_srgb_gamma_is_lossless_only() {
        let bytes = png_with_chunk(&fixture(16, 16, false), *b"gAMA", &100000u32.to_be_bytes());
        let result = optimize_bytes(
            &bytes,
            &options(vec![Format::Png, Format::Webp, Format::Jpeg]),
        )
        .unwrap();
        assert_eq!(result.candidates.len(), 1);
        assert!(result.candidates[0].lossless);
        assert!(result.warnings.iter().any(|w| w.contains("gamma")));
    }
    #[test]
    fn sixteen_bit_png_is_not_quantized_to_eight_bits() {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 16, 16);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Sixteen);
            let mut writer = encoder.write_header().unwrap();
            let data: Vec<u8> = (0u16..256).flat_map(|i| (i * 193).to_be_bytes()).collect();
            writer.write_image_data(&data).unwrap();
            writer.finish().unwrap();
        }
        let result = optimize_bytes(&bytes, &options(vec![Format::Png, Format::Webp])).unwrap();
        assert_eq!(result.candidates.len(), 1);
        assert!(result.candidates[0].lossless);
        if let Some(selected) = result.selected {
            assert!(visually_exact(
                &image::load_from_memory(&bytes).unwrap(),
                &image::load_from_memory(&selected.bytes).unwrap()
            ));
        }
    }
    #[test]
    fn alpha_cannot_be_flattened_even_if_score_target_low() {
        let rgba = fixture(16, 16, true);
        let decoded = DynamicImage::ImageRgba8(rgba.clone());
        let source = Source {
            decoded: &decoded,
            rgba: &rgba,
            has_alpha: true,
            tiny: false,
            target: 0.0,
        };
        let mut flat = rgba.clone();
        for p in flat.pixels_mut() {
            p[3] = 255;
        }
        assert!(
            verify(&source, &encode_png(&flat).unwrap(), false)
                .unwrap_err()
                .contains("flattened")
        );
    }
    #[test]
    fn exif_jpeg_is_retained_without_orientation_loss() {
        let base = encode_codec(Codec::Jpeg, &fixture(16, 16, false), Some(90)).unwrap();
        let mut bytes = base[..2].to_vec();
        bytes.extend_from_slice(&[0xff, 0xe1, 0, 8]);
        bytes.extend_from_slice(b"Exif\0\0");
        bytes.extend_from_slice(&base[2..]);
        let result = optimize_bytes(&bytes, &OptimizeOptions::default()).unwrap();
        assert!(result.selected.is_none());
        assert!(result.skip_reason.unwrap().contains("orientation"));
    }
    #[test]
    fn avif_is_explicitly_reported_not_fake_encoded() {
        let bytes = encode_png(&fixture(8, 8, false)).unwrap();
        let result = optimize_bytes(&bytes, &options(vec![Format::Avif])).unwrap();
        assert!(result.selected.is_none());
        assert!(result.candidates.is_empty());
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("AVIF") && w.contains("unsupported"))
        );
    }
    #[test]
    fn corrupt_input_is_rejected() {
        assert!(optimize_bytes(b"not an image", &OptimizeOptions::default()).is_err());
        let mut bytes = encode_png(&fixture(8, 8, false)).unwrap();
        bytes.truncate(bytes.len() - 8);
        assert!(optimize_bytes(&bytes, &OptimizeOptions::default()).is_err());
    }
    #[test]
    fn output_obeys_dimensions_size_alpha_and_score() {
        let rgba = fixture(32, 32, true);
        let input = encode_png(&rgba).unwrap();
        let opts = OptimizeOptions {
            score: 70.0,
            min_saving: 0.0,
            ..OptimizeOptions::default()
        };
        let result = optimize_bytes(&input, &opts).unwrap();
        assert!(!result.candidates.is_empty());
        if let Some(selected) = result.selected {
            assert!(selected.bytes.len() < input.len());
            let output = image::load_from_memory(&selected.bytes).unwrap().to_rgba8();
            assert_eq!(output.dimensions(), rgba.dimensions());
            assert!(output.pixels().any(|p| p[3] < 255));
            assert!(perceptual_score(&rgba, &output).unwrap() >= 70.0);
        }
    }
    #[test]
    fn keep_format_filters_candidates() {
        let bytes = encode_png(&fixture(16, 16, false)).unwrap();
        let opts = OptimizeOptions {
            keep_format: true,
            ..OptimizeOptions::default()
        };
        let result = optimize_bytes(&bytes, &opts).unwrap();
        assert!(result.candidates.iter().all(|c| c.format == Format::Png));
    }
    #[test]
    fn invalid_numeric_options_rejected() {
        let bytes = encode_png(&fixture(8, 8, false)).unwrap();
        for score in [f64::NAN, f64::INFINITY, -1.0, 101.0] {
            assert!(
                optimize_bytes(
                    &bytes,
                    &OptimizeOptions {
                        score,
                        ..OptimizeOptions::default()
                    }
                )
                .is_err()
            );
        }
        assert!(
            optimize_bytes(
                &bytes,
                &OptimizeOptions {
                    min_saving: 101.0,
                    ..OptimizeOptions::default()
                }
            )
            .is_err()
        );
    }
    #[test]
    fn strip_all_keeps_unmanaged_color_source_untouched() {
        let bytes = png_with_chunk(&fixture(16, 16, false), *b"gAMA", &100000u32.to_be_bytes());
        let opts = OptimizeOptions {
            metadata: MetadataPolicy::StripAll,
            ..OptimizeOptions::default()
        };
        let result = optimize_bytes(&bytes, &opts).unwrap();
        assert!(result.selected.is_none());
        assert!(result.candidates.is_empty());
        assert!(result.skip_reason.unwrap().contains("displayed colors"));
    }
    #[test]
    fn animated_png_is_retained() {
        let mut control = 1u32.to_be_bytes().to_vec();
        control.extend_from_slice(&0u32.to_be_bytes());
        let bytes = png_with_chunk(&fixture(16, 16, false), *b"acTL", &control);
        let result = optimize_bytes(&bytes, &OptimizeOptions::default()).unwrap();
        assert!(result.selected.is_none());
        assert!(result.skip_reason.unwrap().contains("animated"));
    }
    #[test]
    fn animated_webp_flag_is_recognized_before_processing() {
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&22u32.to_le_bytes());
        bytes.extend_from_slice(b"WEBPVP8X");
        bytes.extend_from_slice(&10u32.to_le_bytes());
        bytes.extend_from_slice(&[2, 0, 0, 0, 15, 0, 0, 15, 0, 0]);
        assert!(
            inspect_container(&bytes, Format::Webp)
                .unwrap()
                .skip
                .unwrap()
                .contains("animated")
        );
    }
    #[test]
    fn grayscale_and_palette_sources_follow_valid_decoding_path() {
        let rgba = RgbaImage::from_fn(16, 16, |x, y| Rgba([((x + y) * 7) as u8; 4]));
        let palette = encode_codec(Codec::PngQuant, &rgba, Some(100)).unwrap();
        let result = optimize_bytes(&palette, &options(vec![Format::Webp])).unwrap();
        assert!(!result.candidates.is_empty());
        assert!(
            result
                .candidates
                .iter()
                .any(|c| c.encoder == "webp-lossless" && c.accepted)
        );
        let mut gray = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut gray, 16, 16);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer
                .write_image_data(&(0u8..=255).collect::<Vec<_>>())
                .unwrap();
            writer.finish().unwrap();
        }
        let result = optimize_bytes(&gray, &options(vec![Format::Webp])).unwrap();
        assert!(
            result
                .candidates
                .iter()
                .any(|c| c.encoder == "webp-lossless" && c.accepted)
        );
    }
    #[test]
    fn oversized_header_is_rejected_before_pixel_allocation() {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 8001, 1);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&vec![0; 8001 * 3]).unwrap();
            writer.finish().unwrap();
        }
        let result = optimize_bytes(&bytes, &OptimizeOptions::default()).unwrap();
        assert!(result.selected.is_none());
        assert!(result.skip_reason.unwrap().contains("safety limits"));
    }
    #[test]
    fn gamma_45455_alone_does_not_claim_exact_srgb() {
        let bytes = png_with_chunk(&fixture(16, 16, false), *b"gAMA", &45455u32.to_be_bytes());
        let result = optimize_bytes(&bytes, &OptimizeOptions::default()).unwrap();
        assert_eq!(result.candidates.len(), 1);
        assert!(result.candidates[0].lossless);
        assert!(result.warnings.iter().any(|w| w.contains("gamma")));
    }
    #[test]
    fn encoder_errors_are_not_normal_quality_rejections() {
        let mut failed = candidate(Format::Png, 0).summary;
        failed.accepted = false;
        failed.score = None;
        failed.note = Some("synthetic encoder failure".into());
        assert!(
            all_candidates_failed(&[failed.clone()])
                .unwrap()
                .contains("synthetic encoder failure")
        );
        let mut under_target = failed.clone();
        under_target.score = Some(70.0);
        under_target.size = 400;
        assert!(all_candidates_failed(&[failed.clone(), under_target]).is_none());
        assert!(all_candidates_failed(&[failed, candidate(Format::Webp, 200).summary]).is_none());
        assert!(all_candidates_failed(&[]).is_none());
    }
    #[test]
    fn lossless_score_clearly_discloses_identity_proof() {
        let bytes = encode_png(&fixture(8, 8, false)).unwrap();
        let opts = OptimizeOptions {
            metadata: MetadataPolicy::Keep,
            ..OptimizeOptions::default()
        };
        let result = optimize_bytes(&bytes, &opts).unwrap();
        assert!(
            result.candidates[0]
                .note
                .as_ref()
                .unwrap()
                .contains("not measured")
        );
        assert!(result.runtime_error.is_none());
    }
    #[test]
    fn subtle_sixteen_bit_transparency_is_not_rounded_away() {
        let image = DynamicImage::ImageRgba16(ImageBuffer::from_pixel(
            8,
            8,
            Rgba([40000u16, 30000, 20000, 65534]),
        ));
        assert!(has_transparency(&image));
        assert!(!has_transparency(&DynamicImage::ImageRgba8(
            image.to_rgba8()
        )));
    }
}
