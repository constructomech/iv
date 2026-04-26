use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::app::DecodedImage;

const XMP_READ_SIZE: usize = 1024 * 1024;
const MAX_XMP_PACKET_SIZE: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XmpDevelopSource {
    Sidecar,
    Embedded,
}

impl XmpDevelopSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Sidecar => "Sidecar XMP",
            Self::Embedded => "Embedded XMP",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmpDevelopSetting {
    pub name: String,
    pub value: String,
    pub has_effect: bool,
    pub applied: bool,
    pub unsupported: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct XmpDevelopSettings {
    pub source: Option<XmpDevelopSource>,
    pub settings: Vec<XmpDevelopSetting>,
}

impl XmpDevelopSettings {
    pub fn is_empty(&self) -> bool {
        self.settings.is_empty()
    }

    pub fn has_visible_settings(&self) -> bool {
        self.settings.iter().any(|setting| setting.has_effect)
    }

    pub fn visible_settings(&self) -> impl Iterator<Item = &XmpDevelopSetting> {
        self.settings.iter().filter(|setting| setting.has_effect)
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.settings
            .iter()
            .find(|setting| setting.name == name)
            .map(|setting| setting.value.as_str())
    }

    fn get_f32(&self, name: &str) -> Option<f32> {
        self.get(name).and_then(parse_xmp_f32)
    }

    fn get_bool(&self, name: &str) -> Option<bool> {
        self.get(name).and_then(parse_xmp_bool)
    }

    fn set(&mut self, name: &str, value: String) {
        let value = decode_xml_entities(value.trim().trim_matches('\0').trim());
        if value.is_empty() {
            return;
        }
        if let Some(existing) = self
            .settings
            .iter_mut()
            .find(|setting| setting.name == name)
        {
            existing.value = value;
        } else {
            self.settings.push(XmpDevelopSetting {
                name: name.to_string(),
                value,
                has_effect: false,
                applied: false,
                unsupported: false,
            });
        }
    }

    fn refresh_applied_flags(&mut self) {
        let context = DevelopEffectContext::from_settings(self);
        for setting in &mut self.settings {
            let status = classify_develop_setting(&setting.name, &setting.value, &context);
            setting.has_effect = status.has_effect();
            setting.applied = status == DevelopSettingStatus::Applied;
            setting.unsupported = status == DevelopSettingStatus::Unsupported;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DevelopSettingStatus {
    NoEffect,
    Applied,
    Unsupported,
}

impl DevelopSettingStatus {
    fn has_effect(self) -> bool {
        self != Self::NoEffect
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct DevelopEffectContext {
    explicit_tone_curve: bool,
    has_parametric_tone: bool,
    has_shadow_split_toning: bool,
    has_highlight_split_toning: bool,
    has_vignette: bool,
    has_post_crop_vignette: bool,
    has_grain: bool,
    has_sharpening: bool,
    has_luminance_noise_reduction: bool,
    has_color_noise_reduction: bool,
    lens_profile_enabled: bool,
    has_crop: bool,
}

impl DevelopEffectContext {
    fn from_settings(settings: &XmpDevelopSettings) -> Self {
        let has_parametric_tone = [
            "ParametricShadows",
            "ParametricDarks",
            "ParametricLights",
            "ParametricHighlights",
        ]
        .iter()
        .any(|name| {
            settings
                .get(name)
                .is_some_and(|value| numeric_setting_has_effect(value, 0.0))
        });

        let has_shadow_split_toning = settings
            .get("SplitToningShadowSaturation")
            .is_some_and(numeric_setting_positive);
        let has_highlight_split_toning = settings
            .get("SplitToningHighlightSaturation")
            .is_some_and(numeric_setting_positive);
        let has_vignette = settings
            .get("VignetteAmount")
            .is_some_and(|value| numeric_setting_has_effect(value, 0.0));
        let has_post_crop_vignette = settings
            .get("PostCropVignetteAmount")
            .is_some_and(|value| numeric_setting_has_effect(value, 0.0));
        let has_grain = settings
            .get("GrainAmount")
            .is_some_and(numeric_setting_positive);
        let has_sharpening = settings
            .get("Sharpness")
            .is_some_and(numeric_setting_positive);
        let has_luminance_noise_reduction = settings
            .get("LuminanceSmoothing")
            .or_else(|| settings.get("LuminanceNoiseReduction"))
            .is_some_and(numeric_setting_positive);
        let has_color_noise_reduction = settings
            .get("ColorNoiseReduction")
            .is_some_and(numeric_setting_positive);
        let lens_profile_enabled = settings
            .get("LensProfileEnable")
            .is_some_and(bool_or_numeric_true);
        let has_crop = settings.get("HasCrop").is_some_and(bool_or_numeric_true)
            || settings
                .get("CropTop")
                .is_some_and(|value| numeric_setting_has_effect(value, 0.0))
            || settings
                .get("CropLeft")
                .is_some_and(|value| numeric_setting_has_effect(value, 0.0))
            || settings
                .get("CropBottom")
                .is_some_and(|value| numeric_setting_has_effect(value, 1.0))
            || settings
                .get("CropRight")
                .is_some_and(|value| numeric_setting_has_effect(value, 1.0))
            || settings
                .get("CropAngle")
                .is_some_and(|value| numeric_setting_has_effect(value, 0.0));

        Self {
            explicit_tone_curve: settings
                .get("ToneCurve")
                .and_then(parse_tone_curve_points)
                .is_some(),
            has_parametric_tone,
            has_shadow_split_toning,
            has_highlight_split_toning,
            has_vignette,
            has_post_crop_vignette,
            has_grain,
            has_sharpening,
            has_luminance_noise_reduction,
            has_color_noise_reduction,
            lens_profile_enabled,
            has_crop,
        }
    }
}

/// Read Adobe Camera Raw / Lightroom develop settings for an image.
///
/// A sidecar next to the image wins over embedded XMP because that is how raw
/// editors usually override container metadata. `data` lets callers that have
/// already read the image avoid a second disk read for embedded packets.
pub fn read_xmp_develop_settings_for_image(path: &Path, data: Option<&[u8]>) -> XmpDevelopSettings {
    if let Some(settings) = read_sidecar_develop_settings(path) {
        return settings;
    }

    if let Some(data) = data
        && let Some(text) = extract_xmp_text_from_bytes(data)
    {
        let settings = parse_xmp_develop_settings(&text, XmpDevelopSource::Embedded);
        if !settings.is_empty() {
            return settings;
        }
    }

    read_embedded_develop_settings_from_path(path).unwrap_or_default()
}

/// Read develop settings without requiring the full image bytes.
pub fn read_xmp_develop_settings_from_path(path: &Path) -> XmpDevelopSettings {
    read_xmp_develop_settings_for_image(path, None)
}

/// Apply the subset of develop settings that iv approximates in display space.
pub fn apply_xmp_develop_settings(image: &mut DecodedImage, settings: &XmpDevelopSettings) {
    let adjustments = DevelopAdjustments::from_settings(settings);
    if !adjustments.has_effect() {
        return;
    }

    for pixel in image.pixels.chunks_exact_mut(4) {
        let mut red = srgb_to_linear(pixel[0] as f32 / 255.0);
        let mut green = srgb_to_linear(pixel[1] as f32 / 255.0);
        let mut blue = srgb_to_linear(pixel[2] as f32 / 255.0);

        red *= adjustments.wb_red * adjustments.exposure_scale;
        green *= adjustments.wb_green * adjustments.exposure_scale;
        blue *= adjustments.wb_blue * adjustments.exposure_scale;

        let mut luma = linear_luma(red, green, blue).clamp(0.0, 1.0);
        let shadow_mask = (1.0 - luma).powi(2);
        let highlight_mask = luma.powi(2);

        if adjustments.fill_light != 0.0 {
            let lift = adjustments.fill_light * shadow_mask * 0.75;
            red += (1.0 - red) * lift;
            green += (1.0 - green) * lift;
            blue += (1.0 - blue) * lift;
        }

        if adjustments.shadows != 0.0 {
            let amount = adjustments.shadows * shadow_mask * 0.85;
            if amount > 0.0 {
                let factor = (1.0 - amount).max(0.0);
                red *= factor;
                green *= factor;
                blue *= factor;
            } else {
                let lift = -amount;
                red += (1.0 - red) * lift;
                green += (1.0 - green) * lift;
                blue += (1.0 - blue) * lift;
            }
        }

        if adjustments.highlight_recovery != 0.0 {
            let factor = (1.0 - adjustments.highlight_recovery * highlight_mask * 0.55).max(0.0);
            red *= factor;
            green *= factor;
            blue *= factor;
        }

        if adjustments.shadow_tint != 0.0 {
            let tint = adjustments.shadow_tint * shadow_mask * 0.18;
            if tint > 0.0 {
                red += tint;
                blue += tint;
                green *= 1.0 - tint.min(0.5);
            } else {
                green += -tint;
                red *= 1.0 - (-tint).min(0.5);
                blue *= 1.0 - (-tint).min(0.5);
            }
        }

        if adjustments.brightness != 0.0 {
            red += adjustments.brightness;
            green += adjustments.brightness;
            blue += adjustments.brightness;
        }

        if adjustments.has_parametric_tone() {
            luma = linear_luma(red, green, blue).clamp(0.0, 1.0);
            let tone = adjustments.parametric_tone_shift(luma);
            red += tone;
            green += tone;
            blue += tone;
        }

        if adjustments.contrast_factor != 1.0 {
            red = (red - 0.5) * adjustments.contrast_factor + 0.5;
            green = (green - 0.5) * adjustments.contrast_factor + 0.5;
            blue = (blue - 0.5) * adjustments.contrast_factor + 0.5;
        }

        let mut red = linear_to_srgb(red.clamp(0.0, 1.0));
        let mut green = linear_to_srgb(green.clamp(0.0, 1.0));
        let mut blue = linear_to_srgb(blue.clamp(0.0, 1.0));

        if let Some(lut) = &adjustments.tone_curve_lut {
            red = lut[(red * 255.0).round().clamp(0.0, 255.0) as usize];
            green = lut[(green * 255.0).round().clamp(0.0, 255.0) as usize];
            blue = lut[(blue * 255.0).round().clamp(0.0, 255.0) as usize];
        }

        if adjustments.needs_hsl() {
            let (mut hue, mut saturation, mut lightness) = rgb_to_hsl(red, green, blue);

            if adjustments.vibrance != 0.0 {
                saturation *= 1.0 + adjustments.vibrance * (1.0 - saturation) * 1.25;
            }
            if adjustments.saturation != 0.0 {
                saturation *= 1.0 + adjustments.saturation;
            }

            for band in &adjustments.hsl_bands {
                let weight = hue_weight(hue, band.center);
                if weight == 0.0 {
                    continue;
                }
                hue += band.hue * weight;
                saturation *= 1.0 + band.saturation * weight;
                lightness += band.luminance * weight;
            }

            saturation = saturation.clamp(0.0, 1.0);
            lightness = lightness.clamp(0.0, 1.0);
            (red, green, blue) = hsl_to_rgb(hue, saturation, lightness);
        }

        if adjustments.grayscale {
            let gray = (0.2126 * red + 0.7152 * green + 0.0722 * blue).clamp(0.0, 1.0);
            red = gray;
            green = gray;
            blue = gray;
        }

        if adjustments.has_split_toning() {
            let luma = (0.2126 * red + 0.7152 * green + 0.0722 * blue).clamp(0.0, 1.0);
            let pivot = 0.5 + adjustments.split_toning_balance * 0.25;
            let shadow_weight = 1.0 - smoothstep(pivot - 0.35, pivot + 0.05, luma);
            let highlight_weight = smoothstep(pivot - 0.05, pivot + 0.35, luma);
            if adjustments.split_toning_shadow_saturation > 0.0 && shadow_weight > 0.0 {
                (red, green, blue) = mix_tone_color(
                    red,
                    green,
                    blue,
                    adjustments.split_toning_shadow_hue,
                    adjustments.split_toning_shadow_saturation,
                    shadow_weight,
                );
            }
            if adjustments.split_toning_highlight_saturation > 0.0 && highlight_weight > 0.0 {
                (red, green, blue) = mix_tone_color(
                    red,
                    green,
                    blue,
                    adjustments.split_toning_highlight_hue,
                    adjustments.split_toning_highlight_saturation,
                    highlight_weight,
                );
            }
        }

        pixel[0] = (red.clamp(0.0, 1.0) * 255.0).round() as u8;
        pixel[1] = (green.clamp(0.0, 1.0) * 255.0).round() as u8;
        pixel[2] = (blue.clamp(0.0, 1.0) * 255.0).round() as u8;
    }

    if adjustments.clarity != 0.0 {
        apply_clarity(image, adjustments.clarity);
    }
    if adjustments.vignette != 0.0 || adjustments.grain != 0.0 {
        apply_vignette_and_grain(image, &adjustments);
    }
}

fn read_sidecar_develop_settings(path: &Path) -> Option<XmpDevelopSettings> {
    for sidecar in sidecar_candidates(path) {
        let Ok(text) = std::fs::read_to_string(sidecar) else {
            continue;
        };
        let settings = parse_xmp_develop_settings(&text, XmpDevelopSource::Sidecar);
        if !settings.is_empty() {
            return Some(settings);
        }
    }
    None
}

fn sidecar_candidates(path: &Path) -> Vec<std::path::PathBuf> {
    let lower = path.with_extension("xmp");
    let upper = path.with_extension("XMP");
    if lower == upper {
        vec![lower]
    } else {
        vec![lower, upper]
    }
}

fn read_embedded_develop_settings_from_path(path: &Path) -> Option<XmpDevelopSettings> {
    if is_tiff_like_extension(path)
        && let Some(text) = read_tiff_xmp_packet(path)
    {
        let settings = parse_xmp_develop_settings(&text, XmpDevelopSource::Embedded);
        if !settings.is_empty() {
            return Some(settings);
        }
    }

    let data = read_prefix(path, XMP_READ_SIZE)?;
    let text = extract_xmp_text_from_bytes(&data)?;
    let settings = parse_xmp_develop_settings(&text, XmpDevelopSource::Embedded);
    (!settings.is_empty()).then_some(settings)
}

fn parse_xmp_develop_settings(text: &str, source: XmpDevelopSource) -> XmpDevelopSettings {
    let mut settings = XmpDevelopSettings {
        source: Some(source),
        settings: Vec::new(),
    };
    parse_crs_attributes(text, &mut settings);
    parse_crs_elements(text, &mut settings);
    settings.refresh_applied_flags();
    settings
}

fn parse_crs_attributes(text: &str, settings: &mut XmpDevelopSettings) {
    let mut pos = 0;
    while let Some(rel) = text[pos..].find("crs:") {
        let start = pos + rel + 4;
        let Some(name_end) = scan_crs_name(text, start) else {
            pos = start;
            continue;
        };
        let name = &text[start..name_end];
        let after_name = text[name_end..].trim_start();
        let Some(after_equals) = after_name.strip_prefix('=') else {
            pos = name_end;
            continue;
        };
        let after_equals = after_equals.trim_start();
        let Some(quote) = after_equals
            .chars()
            .next()
            .filter(|ch| *ch == '"' || *ch == '\'')
        else {
            pos = name_end;
            continue;
        };
        let value_start = quote.len_utf8();
        if let Some(value_end) = after_equals[value_start..].find(quote) {
            settings.set(
                name,
                after_equals[value_start..value_start + value_end].to_string(),
            );
            pos = name_end + value_start + value_end;
        } else {
            break;
        }
    }
}

fn parse_crs_elements(text: &str, settings: &mut XmpDevelopSettings) {
    let mut pos = 0;
    while let Some(rel) = text[pos..].find("<crs:") {
        let tag_start = pos + rel;
        let name_start = tag_start + 5;
        let Some(name_end) = scan_crs_name(text, name_start) else {
            pos = name_start;
            continue;
        };
        let name = &text[name_start..name_end];
        let Some(open_end_rel) = text[name_end..].find('>') else {
            break;
        };
        let open_end = name_end + open_end_rel;
        if text[name_end..open_end].trim_end().ends_with('/') {
            pos = open_end + 1;
            continue;
        }
        let close_tag = format!("</crs:{name}>");
        let content_start = open_end + 1;
        let Some(close_rel) = text[content_start..].find(&close_tag) else {
            pos = content_start;
            continue;
        };
        let content_end = content_start + close_rel;
        let content = &text[content_start..content_end];
        let value = if content.contains("<rdf:li") {
            parse_rdf_li_values(content).join("; ")
        } else {
            strip_xml_tags(content)
        };
        settings.set(name, value);
        pos = content_end + close_tag.len();
    }
}

fn scan_crs_name(text: &str, start: usize) -> Option<usize> {
    let mut end = start;
    for (offset, ch) in text[start..].char_indices() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            end = start + offset + ch.len_utf8();
        } else {
            break;
        }
    }
    (end > start).then_some(end)
}

fn parse_rdf_li_values(content: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut pos = 0;
    while let Some(rel) = content[pos..].find("<rdf:li") {
        let tag_start = pos + rel;
        let Some(open_end_rel) = content[tag_start..].find('>') else {
            break;
        };
        let value_start = tag_start + open_end_rel + 1;
        let Some(close_rel) = content[value_start..].find("</rdf:li>") else {
            break;
        };
        let value_end = value_start + close_rel;
        let value = decode_xml_entities(strip_xml_tags(&content[value_start..value_end]).trim());
        if !value.is_empty() {
            values.push(value);
        }
        pos = value_end + "</rdf:li>".len();
    }
    values
}

fn strip_xml_tags(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut in_tag = false;
    for ch in text.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => output.push(ch),
            _ => {}
        }
    }
    output.trim().to_string()
}

fn decode_xml_entities(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn extract_xmp_text_from_bytes(data: &[u8]) -> Option<String> {
    let (start, end_marker) = if let Some(start) = find_subslice(data, b"<x:xmpmeta") {
        (start, b"</x:xmpmeta>".as_slice())
    } else if let Some(start) = find_subslice(data, b"<rdf:RDF") {
        (start, b"</rdf:RDF>".as_slice())
    } else {
        return None;
    };
    let search_from = start;
    let end_rel = find_subslice(&data[search_from..], end_marker)?;
    let end = search_from + end_rel + end_marker.len();
    if end - start > MAX_XMP_PACKET_SIZE {
        return None;
    }
    Some(String::from_utf8_lossy(&data[start..end]).to_string())
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn read_tiff_xmp_packet(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut header = [0u8; 8];
    file.read_exact(&mut header).ok()?;
    let le = match &header[..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let magic = read_ordered_u16(&header[2..4], le)?;
    if magic != 42 {
        return None;
    }
    let ifd0 = read_ordered_u32(&header[4..8], le)? as u64;
    read_tiff_xmp_from_ifd(&mut file, ifd0, le, 0)
}

fn read_tiff_xmp_from_ifd(
    file: &mut std::fs::File,
    ifd_offset: u64,
    le: bool,
    depth: usize,
) -> Option<String> {
    if ifd_offset == 0 || depth > 8 {
        return None;
    }
    file.seek(SeekFrom::Start(ifd_offset)).ok()?;
    let mut count_bytes = [0u8; 2];
    file.read_exact(&mut count_bytes).ok()?;
    let count = read_ordered_u16(&count_bytes, le)? as usize;
    let entries_len = count.checked_mul(12)?.checked_add(4)?;
    if entries_len > 1024 * 1024 {
        return None;
    }
    let mut entries = vec![0u8; entries_len];
    file.read_exact(&mut entries).ok()?;

    for i in 0..count {
        let entry = &entries[i * 12..i * 12 + 12];
        let tag = read_ordered_u16(&entry[0..2], le)?;
        if tag != 0x02bc {
            continue;
        }
        let field_type = read_ordered_u16(&entry[2..4], le)?;
        let value_count = read_ordered_u32(&entry[4..8], le)? as usize;
        let type_size = tiff_field_type_size(field_type)?;
        let byte_len = value_count.checked_mul(type_size)?;
        if byte_len > MAX_XMP_PACKET_SIZE {
            return None;
        }
        let bytes = if byte_len <= 4 {
            entry[8..8 + byte_len].to_vec()
        } else {
            let offset = read_ordered_u32(&entry[8..12], le)? as u64;
            read_file_range(file, offset, byte_len)?
        };
        return Some(
            String::from_utf8_lossy(bytes.trim_ascii_end_matches(&[0]))
                .trim()
                .to_string(),
        );
    }

    let next_ifd = read_ordered_u32(&entries[count * 12..count * 12 + 4], le)? as u64;
    read_tiff_xmp_from_ifd(file, next_ifd, le, depth + 1)
}

fn read_file_range(file: &mut std::fs::File, offset: u64, length: usize) -> Option<Vec<u8>> {
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut data = vec![0u8; length];
    file.read_exact(&mut data).ok()?;
    Some(data)
}

fn read_ordered_u16(bytes: &[u8], le: bool) -> Option<u16> {
    let bytes: [u8; 2] = bytes.get(..2)?.try_into().ok()?;
    Some(if le {
        u16::from_le_bytes(bytes)
    } else {
        u16::from_be_bytes(bytes)
    })
}

fn read_ordered_u32(bytes: &[u8], le: bool) -> Option<u32> {
    let bytes: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    Some(if le {
        u32::from_le_bytes(bytes)
    } else {
        u32::from_be_bytes(bytes)
    })
}

fn tiff_field_type_size(field_type: u16) -> Option<usize> {
    match field_type {
        1 | 2 | 6 | 7 => Some(1),
        3 | 8 => Some(2),
        4 | 9 | 11 => Some(4),
        5 | 10 | 12 => Some(8),
        _ => None,
    }
}

trait TrimAsciiEndMatches {
    fn trim_ascii_end_matches(&self, bytes: &[u8]) -> &[u8];
}

impl TrimAsciiEndMatches for [u8] {
    fn trim_ascii_end_matches(&self, bytes: &[u8]) -> &[u8] {
        let mut end = self.len();
        while end > 0 && bytes.contains(&self[end - 1]) {
            end -= 1;
        }
        &self[..end]
    }
}

#[derive(Clone, Copy, Debug)]
struct HslBandAdjustment {
    center: f32,
    hue: f32,
    saturation: f32,
    luminance: f32,
}

impl Default for HslBandAdjustment {
    fn default() -> Self {
        Self {
            center: 0.0,
            hue: 0.0,
            saturation: 0.0,
            luminance: 0.0,
        }
    }
}

#[derive(Debug)]
struct DevelopAdjustments {
    exposure_scale: f32,
    wb_red: f32,
    wb_green: f32,
    wb_blue: f32,
    shadow_tint: f32,
    brightness: f32,
    contrast_factor: f32,
    shadows: f32,
    fill_light: f32,
    highlight_recovery: f32,
    saturation: f32,
    vibrance: f32,
    clarity: f32,
    grayscale: bool,
    vignette: f32,
    grain: f32,
    split_toning_shadow_hue: f32,
    split_toning_shadow_saturation: f32,
    split_toning_highlight_hue: f32,
    split_toning_highlight_saturation: f32,
    split_toning_balance: f32,
    tone_curve_lut: Option<[f32; 256]>,
    parametric: [f32; 4],
    parametric_splits: [f32; 3],
    hsl_bands: [HslBandAdjustment; 8],
}

impl DevelopAdjustments {
    fn from_settings(settings: &XmpDevelopSettings) -> Self {
        let exposure = settings.get_f32("Exposure").unwrap_or(0.0).clamp(-5.0, 5.0);
        let brightness = settings
            .get_f32("Brightness")
            .map(|value| ((value - 50.0) / 100.0).clamp(-1.0, 1.0) * 0.35)
            .unwrap_or(0.0);
        let contrast = settings
            .get_f32("Contrast")
            .map(|value| ((value - 25.0) / 100.0).clamp(-1.0, 1.0))
            .unwrap_or(0.0);
        let shadows = settings
            .get_f32("Shadows")
            .map(|value| ((value - 5.0) / 100.0).clamp(-1.0, 1.0))
            .unwrap_or(0.0);
        let fill_light = settings
            .get_f32("FillLight")
            .map(|value| (value / 100.0).clamp(-1.0, 1.0))
            .unwrap_or(0.0);
        let highlight_recovery = settings
            .get_f32("HighlightRecovery")
            .map(|value| (value / 100.0).clamp(0.0, 1.0))
            .unwrap_or(0.0);
        let saturation = settings
            .get_f32("Saturation")
            .map(|value| (value / 100.0).clamp(-1.0, 1.0))
            .unwrap_or(0.0);
        let vibrance = settings
            .get_f32("Vibrance")
            .map(|value| (value / 100.0).clamp(-1.0, 1.0))
            .unwrap_or(0.0);
        let clarity = settings
            .get_f32("Clarity")
            .map(|value| (value / 100.0).clamp(-1.0, 1.0))
            .unwrap_or(0.0);
        let vignette = (settings
            .get_f32("VignetteAmount")
            .map(|value| (value / 100.0).clamp(-1.0, 1.0))
            .unwrap_or(0.0)
            + settings
                .get_f32("PostCropVignetteAmount")
                .map(|value| (value / 100.0).clamp(-1.0, 1.0))
                .unwrap_or(0.0))
        .clamp(-1.0, 1.0);
        let grain = settings
            .get_f32("GrainAmount")
            .map(|value| (value / 100.0).clamp(0.0, 1.0))
            .unwrap_or(0.0);

        let (wb_red, wb_green, wb_blue) = white_balance_multipliers(settings);
        let shadow_tint = settings
            .get_f32("ShadowTint")
            .map(|value| (value / 100.0).clamp(-1.0, 1.0))
            .unwrap_or(0.0);

        let tone_curve_lut = settings
            .get("ToneCurve")
            .and_then(parse_tone_curve_points)
            .or_else(|| {
                settings
                    .get("ToneCurveName")
                    .and_then(tone_curve_preset_points)
            })
            .and_then(build_tone_curve_lut)
            .filter(|lut| !is_identity_lut(lut));

        let parametric = [
            settings.get_f32("ParametricShadows").unwrap_or(0.0) / 100.0 * 0.28,
            settings.get_f32("ParametricDarks").unwrap_or(0.0) / 100.0 * 0.24,
            settings.get_f32("ParametricLights").unwrap_or(0.0) / 100.0 * 0.24,
            settings.get_f32("ParametricHighlights").unwrap_or(0.0) / 100.0 * 0.28,
        ];
        let parametric_splits = [
            settings.get_f32("ParametricShadowSplit").unwrap_or(25.0) / 100.0,
            settings.get_f32("ParametricMidtoneSplit").unwrap_or(50.0) / 100.0,
            settings.get_f32("ParametricHighlightSplit").unwrap_or(75.0) / 100.0,
        ];

        let mut hsl_bands = [HslBandAdjustment::default(); 8];
        for (idx, (suffix, center)) in HSL_COLOR_BANDS.iter().enumerate() {
            hsl_bands[idx] = HslBandAdjustment {
                center: *center,
                hue: settings
                    .get_f32(&format!("HueAdjustment{suffix}"))
                    .map(|value| (value / 100.0).clamp(-1.0, 1.0) * 30.0)
                    .unwrap_or(0.0),
                saturation: settings
                    .get_f32(&format!("SaturationAdjustment{suffix}"))
                    .map(|value| (value / 100.0).clamp(-1.0, 1.0))
                    .unwrap_or(0.0),
                luminance: settings
                    .get_f32(&format!("LuminanceAdjustment{suffix}"))
                    .map(|value| (value / 100.0).clamp(-1.0, 1.0) * 0.25)
                    .unwrap_or(0.0),
            };
        }

        Self {
            exposure_scale: 2.0_f32.powf(exposure),
            wb_red,
            wb_green,
            wb_blue,
            shadow_tint,
            brightness,
            contrast_factor: 1.0 + contrast * 0.85,
            shadows,
            fill_light,
            highlight_recovery,
            saturation,
            vibrance,
            clarity,
            grayscale: settings.get_bool("ConvertToGrayscale").unwrap_or(false),
            vignette,
            grain,
            split_toning_shadow_hue: settings.get_f32("SplitToningShadowHue").unwrap_or(0.0),
            split_toning_shadow_saturation: settings
                .get_f32("SplitToningShadowSaturation")
                .map(|value| (value / 100.0).clamp(0.0, 1.0))
                .unwrap_or(0.0),
            split_toning_highlight_hue: settings.get_f32("SplitToningHighlightHue").unwrap_or(0.0),
            split_toning_highlight_saturation: settings
                .get_f32("SplitToningHighlightSaturation")
                .map(|value| (value / 100.0).clamp(0.0, 1.0))
                .unwrap_or(0.0),
            split_toning_balance: settings
                .get_f32("SplitToningBalance")
                .map(|value| (value / 100.0).clamp(-1.0, 1.0))
                .unwrap_or(0.0),
            tone_curve_lut,
            parametric,
            parametric_splits,
            hsl_bands,
        }
    }

    fn has_effect(&self) -> bool {
        (self.exposure_scale - 1.0).abs() > f32::EPSILON
            || (self.wb_red - 1.0).abs() > f32::EPSILON
            || (self.wb_green - 1.0).abs() > f32::EPSILON
            || (self.wb_blue - 1.0).abs() > f32::EPSILON
            || self.shadow_tint.abs() > f32::EPSILON
            || self.brightness.abs() > f32::EPSILON
            || (self.contrast_factor - 1.0).abs() > f32::EPSILON
            || self.shadows.abs() > f32::EPSILON
            || self.fill_light.abs() > f32::EPSILON
            || self.highlight_recovery.abs() > f32::EPSILON
            || self.saturation.abs() > f32::EPSILON
            || self.vibrance.abs() > f32::EPSILON
            || self.clarity.abs() > f32::EPSILON
            || self.grayscale
            || self.vignette.abs() > f32::EPSILON
            || self.grain.abs() > f32::EPSILON
            || self.has_split_toning()
            || self.tone_curve_lut.is_some()
            || self.has_parametric_tone()
            || self.hsl_bands.iter().any(|band| {
                band.hue.abs() > f32::EPSILON
                    || band.saturation.abs() > f32::EPSILON
                    || band.luminance.abs() > f32::EPSILON
            })
    }

    fn has_parametric_tone(&self) -> bool {
        self.parametric
            .iter()
            .any(|value| value.abs() > f32::EPSILON)
    }

    fn needs_hsl(&self) -> bool {
        self.grayscale
            || self.saturation.abs() > f32::EPSILON
            || self.vibrance.abs() > f32::EPSILON
            || self.hsl_bands.iter().any(|band| {
                band.hue.abs() > f32::EPSILON
                    || band.saturation.abs() > f32::EPSILON
                    || band.luminance.abs() > f32::EPSILON
            })
    }

    fn has_split_toning(&self) -> bool {
        self.split_toning_shadow_saturation > f32::EPSILON
            || self.split_toning_highlight_saturation > f32::EPSILON
    }

    fn parametric_tone_shift(&self, luma: f32) -> f32 {
        let shadow_split = self.parametric_splits[0].clamp(0.05, 0.9);
        let midtone_split = self.parametric_splits[1].clamp(shadow_split + 0.05, 0.95);
        let highlight_split = self.parametric_splits[2].clamp(midtone_split + 0.05, 0.98);

        let shadows = 1.0 - smoothstep(0.0, shadow_split, luma);
        let darks = smoothstep(0.0, shadow_split, luma)
            * (1.0 - smoothstep(shadow_split, midtone_split, luma));
        let lights = smoothstep(shadow_split, midtone_split, luma)
            * (1.0 - smoothstep(midtone_split, highlight_split, luma));
        let highlights = smoothstep(midtone_split, highlight_split, luma);

        self.parametric[0] * shadows
            + self.parametric[1] * darks
            + self.parametric[2] * lights
            + self.parametric[3] * highlights
    }
}

const HSL_COLOR_BANDS: [(&str, f32); 8] = [
    ("Red", 0.0),
    ("Orange", 30.0),
    ("Yellow", 60.0),
    ("Green", 120.0),
    ("Aqua", 180.0),
    ("Blue", 240.0),
    ("Purple", 280.0),
    ("Magenta", 320.0),
];

fn parse_xmp_f32(value: &str) -> Option<f32> {
    let value = value.trim().trim_start_matches('+');
    if let Some((numerator, denominator)) = value.split_once('/') {
        let numerator = numerator.trim().parse::<f32>().ok()?;
        let denominator = denominator.trim().parse::<f32>().ok()?;
        if denominator != 0.0 {
            return Some(numerator / denominator);
        }
    }
    value.parse::<f32>().ok()
}

fn parse_xmp_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

fn classify_develop_setting(
    name: &str,
    value: &str,
    context: &DevelopEffectContext,
) -> DevelopSettingStatus {
    if implemented_setting_has_effect(name, value, context) {
        DevelopSettingStatus::Applied
    } else if unsupported_setting_has_effect(name, value, context) {
        DevelopSettingStatus::Unsupported
    } else {
        DevelopSettingStatus::NoEffect
    }
}

fn implemented_setting_has_effect(name: &str, value: &str, context: &DevelopEffectContext) -> bool {
    match name {
        "Exposure" => numeric_setting_has_effect(value, 0.0),
        "Brightness" => numeric_setting_has_effect(value, 50.0),
        "Contrast" => numeric_setting_has_effect(value, 25.0),
        "Shadows" => numeric_setting_has_effect(value, 5.0),
        "FillLight"
        | "HighlightRecovery"
        | "Clarity"
        | "Saturation"
        | "Vibrance"
        | "Tint"
        | "ShadowTint"
        | "VignetteAmount"
        | "PostCropVignetteAmount"
        | "GrainAmount"
        | "SplitToningShadowSaturation"
        | "SplitToningHighlightSaturation"
        | "ParametricShadows"
        | "ParametricDarks"
        | "ParametricLights"
        | "ParametricHighlights" => numeric_setting_has_effect(value, 0.0),
        "SplitToningShadowHue" => context.has_shadow_split_toning,
        "SplitToningHighlightHue" => context.has_highlight_split_toning,
        "SplitToningBalance" => {
            (context.has_shadow_split_toning || context.has_highlight_split_toning)
                && numeric_setting_has_effect(value, 0.0)
        }
        "Temperature" => numeric_setting_has_effect(value, 5500.0),
        "ConvertToGrayscale" => parse_xmp_bool(value).unwrap_or(false),
        "ToneCurve" => tone_curve_has_effect(value),
        "ToneCurveName" => !context.explicit_tone_curve && tone_curve_name_has_effect(value),
        "ParametricShadowSplit" => {
            context.has_parametric_tone && numeric_setting_has_effect(value, 25.0)
        }
        "ParametricMidtoneSplit" => {
            context.has_parametric_tone && numeric_setting_has_effect(value, 50.0)
        }
        "ParametricHighlightSplit" => {
            context.has_parametric_tone && numeric_setting_has_effect(value, 75.0)
        }
        "WhiteBalance" => {
            white_balance_preset_kelvin(value).is_some_and(|kelvin| (kelvin - 5500.0).abs() > 0.001)
        }
        _ if hsl_setting_name(name) => numeric_setting_has_effect(value, 0.0),
        _ => false,
    }
}

fn unsupported_setting_has_effect(name: &str, value: &str, context: &DevelopEffectContext) -> bool {
    match name {
        "CameraProfile" | "LookName" | "Look" | "Profile" => non_empty_setting(value),
        "ToneCurveName" => {
            !context.explicit_tone_curve
                && !matches!(value.trim().to_ascii_lowercase().as_str(), "" | "linear")
        }
        "Sharpness" => numeric_setting_positive(value),
        "SharpenRadius" => context.has_sharpening && numeric_setting_has_effect(value, 1.0),
        "SharpenDetail" => context.has_sharpening && numeric_setting_has_effect(value, 25.0),
        "SharpenEdgeMasking" => context.has_sharpening && numeric_setting_has_effect(value, 0.0),
        "LuminanceSmoothing" | "LuminanceNoiseReduction" => numeric_setting_positive(value),
        "LuminanceNoiseReductionDetail" => {
            context.has_luminance_noise_reduction && numeric_setting_has_effect(value, 50.0)
        }
        "LuminanceNoiseReductionContrast" => {
            context.has_luminance_noise_reduction && numeric_setting_has_effect(value, 0.0)
        }
        "ColorNoiseReduction" => numeric_setting_positive(value),
        "ColorNoiseReductionDetail" | "ColorNoiseReductionSmoothness" => {
            context.has_color_noise_reduction && numeric_setting_has_effect(value, 50.0)
        }
        "RemoveChromaticAberration" | "Defringe" => bool_or_numeric_true(value),
        "ChromaticAberrationRedHue"
        | "ChromaticAberrationRedSaturation"
        | "ChromaticAberrationBlueHue"
        | "ChromaticAberrationBlueSaturation"
        | "DefringePurpleAmount"
        | "DefringePurpleHueLo"
        | "DefringePurpleHueHi"
        | "DefringeGreenAmount"
        | "DefringeGreenHueLo"
        | "DefringeGreenHueHi" => numeric_setting_has_effect(value, 0.0),
        "LensProfileEnable" => bool_or_numeric_true(value),
        "LensProfileSetup" | "LensProfileName" | "LensProfileFilename" => {
            context.lens_profile_enabled && non_empty_setting(value)
        }
        "LensManualDistortionAmount"
        | "PerspectiveVertical"
        | "PerspectiveHorizontal"
        | "PerspectiveRotate"
        | "PerspectiveAspect"
        | "PerspectiveUpright"
        | "PerspectiveX"
        | "PerspectiveY" => numeric_setting_has_effect(value, 0.0),
        "PerspectiveScale" => numeric_setting_has_effect(value, 100.0),
        "VignetteMidpoint" => context.has_vignette && numeric_setting_has_effect(value, 50.0),
        "PostCropVignetteMidpoint" => {
            context.has_post_crop_vignette && numeric_setting_has_effect(value, 50.0)
        }
        "PostCropVignetteRoundness" | "PostCropVignetteHighlightContrast" => {
            context.has_post_crop_vignette && numeric_setting_has_effect(value, 0.0)
        }
        "PostCropVignetteFeather" => {
            context.has_post_crop_vignette && numeric_setting_has_effect(value, 50.0)
        }
        "PostCropVignetteStyle" => context.has_post_crop_vignette && non_empty_setting(value),
        "GrainSize" => context.has_grain && numeric_setting_has_effect(value, 25.0),
        "GrainFrequency" => context.has_grain && numeric_setting_has_effect(value, 50.0),
        "HasCrop" => bool_or_numeric_true(value),
        "CropTop" | "CropLeft" => numeric_setting_has_effect(value, 0.0),
        "CropBottom" | "CropRight" => numeric_setting_has_effect(value, 1.0),
        "CropAngle" | "CropConstrainToWarp" => numeric_setting_has_effect(value, 0.0),
        "Orientation" | "Rotation" => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "normal"
        ),
        "RetouchInfo"
        | "RetouchAreas"
        | "RedEyeInfo"
        | "PaintBasedCorrections"
        | "CorrectionMasks"
        | "MaskGroupBasedCorrections" => non_empty_setting(value),
        _ if context.has_crop && name.starts_with("Crop") => non_empty_setting(value),
        _ => false,
    }
}

fn numeric_setting_has_effect(value: &str, neutral: f32) -> bool {
    parse_xmp_f32(value).is_some_and(|value| (value - neutral).abs() > 0.001)
}

fn numeric_setting_positive(value: &str) -> bool {
    parse_xmp_f32(value).is_some_and(|value| value > 0.001)
}

fn bool_or_numeric_true(value: &str) -> bool {
    parse_xmp_bool(value)
        .or_else(|| parse_xmp_f32(value).map(|value| value.abs() > 0.001))
        .unwrap_or(false)
}

fn non_empty_setting(value: &str) -> bool {
    !matches!(value.trim(), "" | "0" | "[]")
}

fn tone_curve_has_effect(value: &str) -> bool {
    parse_tone_curve_points(value)
        .and_then(build_tone_curve_lut)
        .is_some_and(|lut| !is_identity_lut(&lut))
}

fn tone_curve_name_has_effect(value: &str) -> bool {
    tone_curve_preset_points(value)
        .and_then(build_tone_curve_lut)
        .is_some_and(|lut| !is_identity_lut(&lut))
}

fn hsl_setting_name(name: &str) -> bool {
    HSL_COLOR_BANDS.iter().any(|(suffix, _)| {
        name == format!("HueAdjustment{suffix}")
            || name == format!("SaturationAdjustment{suffix}")
            || name == format!("LuminanceAdjustment{suffix}")
    })
}

fn white_balance_multipliers(settings: &XmpDevelopSettings) -> (f32, f32, f32) {
    let temperature = settings.get_f32("Temperature").or_else(|| {
        settings
            .get("WhiteBalance")
            .and_then(white_balance_preset_kelvin)
    });
    let tint = settings.get_f32("Tint").unwrap_or(0.0).clamp(-150.0, 150.0) / 150.0;

    let mut red = 1.0;
    let mut green = 1.0;
    let mut blue = 1.0;

    if let Some(temperature) = temperature {
        let warmth = ((temperature - 5500.0) / 5500.0).clamp(-1.0, 1.0);
        red *= 1.0 + warmth * 0.38;
        blue *= 1.0 - warmth * 0.38;
    }

    if tint != 0.0 {
        green *= 1.0 - tint * 0.28;
        red *= 1.0 + tint * 0.08;
        blue *= 1.0 + tint * 0.08;
    }

    (
        red.clamp(0.45, 1.8),
        green.clamp(0.45, 1.8),
        blue.clamp(0.45, 1.8),
    )
}

fn white_balance_preset_kelvin(value: &str) -> Option<f32> {
    match value.trim().to_ascii_lowercase().as_str() {
        "daylight" => Some(5500.0),
        "cloudy" => Some(6500.0),
        "shade" => Some(7500.0),
        "tungsten" => Some(2850.0),
        "fluorescent" => Some(3800.0),
        "flash" => Some(5500.0),
        _ => None,
    }
}

fn parse_tone_curve_points(value: &str) -> Option<Vec<(f32, f32)>> {
    let mut points = Vec::new();
    for part in value.split(';') {
        let Some((x, y)) = part.split_once(',') else {
            continue;
        };
        let x = parse_xmp_f32(x)?.clamp(0.0, 255.0);
        let y = parse_xmp_f32(y)?.clamp(0.0, 255.0);
        points.push((x, y));
    }
    (points.len() >= 2).then_some(points)
}

fn tone_curve_preset_points(value: &str) -> Option<Vec<(f32, f32)>> {
    match value.trim().to_ascii_lowercase().as_str() {
        "linear" => Some(vec![(0.0, 0.0), (255.0, 255.0)]),
        "medium contrast" => Some(vec![
            (0.0, 0.0),
            (32.0, 22.0),
            (64.0, 56.0),
            (128.0, 128.0),
            (192.0, 196.0),
            (255.0, 255.0),
        ]),
        "strong contrast" => Some(vec![
            (0.0, 0.0),
            (32.0, 16.0),
            (64.0, 50.0),
            (128.0, 128.0),
            (192.0, 202.0),
            (255.0, 255.0),
        ]),
        _ => None,
    }
}

fn build_tone_curve_lut(mut points: Vec<(f32, f32)>) -> Option<[f32; 256]> {
    points.sort_by(|a, b| a.0.total_cmp(&b.0));
    if points.first()?.0 > 0.0 {
        points.insert(0, (0.0, 0.0));
    }
    if points.last()?.0 < 255.0 {
        points.push((255.0, 255.0));
    }

    let mut lut = [0.0; 256];
    let mut segment = 0;
    for (i, output) in lut.iter_mut().enumerate() {
        let x = i as f32;
        while segment + 1 < points.len() && points[segment + 1].0 < x {
            segment += 1;
        }
        let (x0, y0) = points[segment];
        let (x1, y1) = points[(segment + 1).min(points.len() - 1)];
        let t = if (x1 - x0).abs() < f32::EPSILON {
            0.0
        } else {
            ((x - x0) / (x1 - x0)).clamp(0.0, 1.0)
        };
        *output = ((y0 + (y1 - y0) * t) / 255.0).clamp(0.0, 1.0);
    }
    Some(lut)
}

fn is_identity_lut(lut: &[f32; 256]) -> bool {
    lut.iter()
        .enumerate()
        .all(|(idx, value)| (*value - idx as f32 / 255.0).abs() < 0.001)
}

fn srgb_to_linear(value: f32) -> f32 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(value: f32) -> f32 {
    if value <= 0.003_130_8 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    }
}

fn linear_luma(red: f32, green: f32, blue: f32) -> f32 {
    0.2126 * red + 0.7152 * green + 0.0722 * blue
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn hue_weight(hue: f32, center: f32) -> f32 {
    let distance = ((hue - center + 180.0).rem_euclid(360.0) - 180.0).abs();
    (1.0 - distance / 45.0).clamp(0.0, 1.0)
}

fn mix_tone_color(
    red: f32,
    green: f32,
    blue: f32,
    hue: f32,
    saturation: f32,
    weight: f32,
) -> (f32, f32, f32) {
    let luma = (0.2126 * red + 0.7152 * green + 0.0722 * blue).clamp(0.0, 1.0);
    let (tone_red, tone_green, tone_blue) = hsl_to_rgb(hue, saturation.clamp(0.0, 1.0), luma);
    let amount = (saturation * weight * 0.45).clamp(0.0, 0.6);
    (
        red + (tone_red - red) * amount,
        green + (tone_green - green) * amount,
        blue + (tone_blue - blue) * amount,
    )
}

fn apply_clarity(image: &mut DecodedImage, amount: f32) {
    let width = image.width as usize;
    let height = image.height as usize;
    if width < 3 || height < 3 {
        return;
    }

    let source = image.pixels.clone();
    let strength = amount.clamp(-1.0, 1.0) * 0.85;
    for y in 1..height - 1 {
        for x in 1..width - 1 {
            let idx = (y * width + x) * 4;
            let center = srgb_luma_u8(&source[idx..idx + 3]);
            let left = srgb_luma_u8(&source[idx - 4..idx - 1]);
            let right = srgb_luma_u8(&source[idx + 4..idx + 7]);
            let up = srgb_luma_u8(&source[idx - width * 4..idx - width * 4 + 3]);
            let down = srgb_luma_u8(&source[idx + width * 4..idx + width * 4 + 3]);
            let local_average = (left + right + up + down) * 0.25;
            let detail = (center - local_average) * strength;
            for channel in &mut image.pixels[idx..idx + 3] {
                let value = (*channel as f32 / 255.0 + detail).clamp(0.0, 1.0);
                *channel = (value * 255.0).round() as u8;
            }
        }
    }
}

fn apply_vignette_and_grain(image: &mut DecodedImage, adjustments: &DevelopAdjustments) {
    let width = image.width as usize;
    let height = image.height as usize;
    if width == 0 || height == 0 {
        return;
    }

    let center_x = (width.saturating_sub(1)) as f32 * 0.5;
    let center_y = (height.saturating_sub(1)) as f32 * 0.5;
    let max_radius = (center_x * center_x + center_y * center_y).sqrt().max(1.0);

    for y in 0..height {
        for x in 0..width {
            let idx = (y * width + x) * 4;
            let dx = x as f32 - center_x;
            let dy = y as f32 - center_y;
            let distance = (dx * dx + dy * dy).sqrt() / max_radius;
            let edge_mask = smoothstep(0.35, 1.0, distance);

            if adjustments.vignette != 0.0 && edge_mask > 0.0 {
                let amount = adjustments.vignette * edge_mask;
                for channel in &mut image.pixels[idx..idx + 3] {
                    let value = *channel as f32 / 255.0;
                    let adjusted = if amount < 0.0 {
                        value * (1.0 + amount * 0.75).max(0.0)
                    } else {
                        value + (1.0 - value) * amount * 0.55
                    };
                    *channel = (adjusted.clamp(0.0, 1.0) * 255.0).round() as u8;
                }
            }

            if adjustments.grain > 0.0 {
                let noise = hash_noise_2d(x as u32, y as u32) - 0.5;
                let amount = noise * adjustments.grain * 38.0;
                for channel in &mut image.pixels[idx..idx + 3] {
                    *channel = (*channel as f32 + amount).round().clamp(0.0, 255.0) as u8;
                }
            }
        }
    }
}

fn srgb_luma_u8(rgb: &[u8]) -> f32 {
    (0.2126 * rgb[0] as f32 + 0.7152 * rgb[1] as f32 + 0.0722 * rgb[2] as f32) / 255.0
}

fn hash_noise_2d(x: u32, y: u32) -> f32 {
    let mut value = x.wrapping_mul(0x9e37_79b1) ^ y.wrapping_mul(0x85eb_ca6b);
    value ^= value >> 16;
    value = value.wrapping_mul(0x7feb_352d);
    value ^= value >> 15;
    value = value.wrapping_mul(0x846c_a68b);
    value ^= value >> 16;
    value as f32 / u32::MAX as f32
}

fn rgb_to_hsl(red: f32, green: f32, blue: f32) -> (f32, f32, f32) {
    let max = red.max(green).max(blue);
    let min = red.min(green).min(blue);
    let lightness = (max + min) * 0.5;
    if (max - min).abs() < f32::EPSILON {
        return (0.0, 0.0, lightness);
    }

    let delta = max - min;
    let saturation = if lightness > 0.5 {
        delta / (2.0 - max - min)
    } else {
        delta / (max + min)
    };

    let hue = if (max - red).abs() < f32::EPSILON {
        60.0 * ((green - blue) / delta).rem_euclid(6.0)
    } else if (max - green).abs() < f32::EPSILON {
        60.0 * ((blue - red) / delta + 2.0)
    } else {
        60.0 * ((red - green) / delta + 4.0)
    };

    (hue.rem_euclid(360.0), saturation, lightness)
}

fn hsl_to_rgb(hue: f32, saturation: f32, lightness: f32) -> (f32, f32, f32) {
    if saturation <= f32::EPSILON {
        return (lightness, lightness, lightness);
    }

    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let hue_prime = hue.rem_euclid(360.0) / 60.0;
    let x = chroma * (1.0 - (hue_prime.rem_euclid(2.0) - 1.0).abs());
    let (red1, green1, blue1) = if hue_prime < 1.0 {
        (chroma, x, 0.0)
    } else if hue_prime < 2.0 {
        (x, chroma, 0.0)
    } else if hue_prime < 3.0 {
        (0.0, chroma, x)
    } else if hue_prime < 4.0 {
        (0.0, x, chroma)
    } else if hue_prime < 5.0 {
        (x, 0.0, chroma)
    } else {
        (chroma, 0.0, x)
    };
    let m = lightness - chroma * 0.5;
    (red1 + m, green1 + m, blue1 + m)
}

fn read_prefix(path: &Path, max_len: usize) -> Option<Vec<u8>> {
    let mut file = std::fs::File::open(path).ok()?;
    let read_len = file.metadata().ok()?.len().min(max_len as u64) as usize;
    let mut data = vec![0; read_len];
    file.read_exact(&mut data).ok()?;
    Some(data)
}

fn is_tiff_like_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| matches!(ext.to_ascii_lowercase().as_str(), "dng" | "tif" | "tiff"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_test_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("iv_develop_test_{name}_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn xmp_develop_parser_reads_elements_and_marks_applied_settings() {
        let text = r#"
            <x:xmpmeta><rdf:RDF><rdf:Description xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/">
                <crs:Exposure>+1.00</crs:Exposure>
                <crs:Brightness>+65</crs:Brightness>
                <crs:Clarity>20</crs:Clarity>
                <crs:VignetteAmount>-30</crs:VignetteAmount>
                <crs:SplitToningShadowSaturation>25</crs:SplitToningShadowSaturation>
                <crs:GrainAmount>10</crs:GrainAmount>
                <crs:CameraProfile>ACR 3.4</crs:CameraProfile>
                <crs:Sharpness>25</crs:Sharpness>
                <crs:ToneCurve><rdf:Seq><rdf:li>0, 0</rdf:li><rdf:li>255, 255</rdf:li></rdf:Seq></crs:ToneCurve>
            </rdf:Description></rdf:RDF></x:xmpmeta>
        "#;

        let settings = parse_xmp_develop_settings(text, XmpDevelopSource::Sidecar);

        assert_eq!(settings.source, Some(XmpDevelopSource::Sidecar));
        assert_eq!(settings.get("Exposure"), Some("+1.00"));
        assert_eq!(settings.get("ToneCurve"), Some("0, 0; 255, 255"));
        assert!(
            settings
                .settings
                .iter()
                .find(|setting| setting.name == "Exposure")
                .is_some_and(|setting| setting.has_effect && setting.applied)
        );
        assert!(
            settings
                .settings
                .iter()
                .find(|setting| setting.name == "Brightness")
                .is_some_and(|setting| setting.has_effect && setting.applied)
        );
        assert!(
            settings
                .settings
                .iter()
                .find(|setting| setting.name == "Clarity")
                .is_some_and(|setting| setting.has_effect && setting.applied)
        );
        assert!(
            settings
                .settings
                .iter()
                .find(|setting| setting.name == "VignetteAmount")
                .is_some_and(|setting| setting.has_effect && setting.applied)
        );
        assert!(
            settings
                .settings
                .iter()
                .find(|setting| setting.name == "Sharpness")
                .is_some_and(|setting| setting.has_effect && setting.unsupported)
        );
        assert!(
            settings
                .settings
                .iter()
                .find(|setting| setting.name == "CameraProfile")
                .is_some_and(|setting| setting.has_effect && setting.unsupported)
        );
        assert!(
            settings
                .settings
                .iter()
                .find(|setting| setting.name == "ToneCurve")
                .is_some_and(|setting| !setting.has_effect && !setting.applied)
        );
    }

    #[test]
    fn xmp_develop_parser_marks_neutral_settings_unapplied() {
        let text = r#"
            <x:xmpmeta><rdf:RDF><rdf:Description xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/">
                <crs:Exposure>0.00</crs:Exposure>
                <crs:Brightness>+50</crs:Brightness>
                <crs:Contrast>+25</crs:Contrast>
                <crs:Shadows>5</crs:Shadows>
                <crs:Clarity>0</crs:Clarity>
                <crs:ToneCurveName>Linear</crs:ToneCurveName>
                <crs:ToneCurve><rdf:Seq><rdf:li>0, 0</rdf:li><rdf:li>255, 255</rdf:li></rdf:Seq></crs:ToneCurve>
            </rdf:Description></rdf:RDF></x:xmpmeta>
        "#;

        let settings = parse_xmp_develop_settings(text, XmpDevelopSource::Sidecar);

        for name in [
            "Exposure",
            "Brightness",
            "Contrast",
            "Shadows",
            "Clarity",
            "ToneCurveName",
            "ToneCurve",
        ] {
            assert!(
                settings
                    .settings
                    .iter()
                    .find(|setting| setting.name == name)
                    .is_some_and(|setting| !setting.has_effect
                        && !setting.applied
                        && !setting.unsupported),
                "{name} should be neutral"
            );
        }
    }

    #[test]
    fn xmp_develop_parser_marks_unsupported_non_neutral_settings() {
        let text = r#"
            <x:xmpmeta><rdf:RDF><rdf:Description xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/">
                <crs:Sharpness>25</crs:Sharpness>
                <crs:ColorNoiseReduction>25</crs:ColorNoiseReduction>
                <crs:LensProfileEnable>1</crs:LensProfileEnable>
                <crs:HasCrop>True</crs:HasCrop>
                <crs:CropRight>0.8</crs:CropRight>
            </rdf:Description></rdf:RDF></x:xmpmeta>
        "#;

        let settings = parse_xmp_develop_settings(text, XmpDevelopSource::Sidecar);

        for name in [
            "Sharpness",
            "ColorNoiseReduction",
            "LensProfileEnable",
            "HasCrop",
            "CropRight",
        ] {
            assert!(
                settings
                    .settings
                    .iter()
                    .find(|setting| setting.name == name)
                    .is_some_and(|setting| setting.has_effect
                        && !setting.applied
                        && setting.unsupported),
                "{name} should be a visible unsupported edit"
            );
        }
    }

    #[test]
    fn xmp_develop_parser_uses_explicit_tone_curve_over_curve_name() {
        let text = r#"
            <x:xmpmeta><rdf:RDF><rdf:Description xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/">
                <crs:ToneCurveName>Medium Contrast</crs:ToneCurveName>
                <crs:ToneCurve><rdf:Seq><rdf:li>0, 0</rdf:li><rdf:li>32, 22</rdf:li><rdf:li>255, 255</rdf:li></rdf:Seq></crs:ToneCurve>
            </rdf:Description></rdf:RDF></x:xmpmeta>
        "#;

        let settings = parse_xmp_develop_settings(text, XmpDevelopSource::Sidecar);

        assert!(
            settings
                .settings
                .iter()
                .find(|setting| setting.name == "ToneCurve")
                .is_some_and(|setting| setting.has_effect && setting.applied)
        );
        assert!(
            settings
                .settings
                .iter()
                .find(|setting| setting.name == "ToneCurveName")
                .is_some_and(|setting| !setting.has_effect && !setting.applied)
        );
    }

    #[test]
    fn xmp_develop_parser_uses_curve_name_when_curve_is_absent() {
        let text = r#"
            <x:xmpmeta><rdf:RDF><rdf:Description xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/">
                <crs:ToneCurveName>Medium Contrast</crs:ToneCurveName>
            </rdf:Description></rdf:RDF></x:xmpmeta>
        "#;

        let settings = parse_xmp_develop_settings(text, XmpDevelopSource::Sidecar);

        assert!(
            settings
                .settings
                .iter()
                .find(|setting| setting.name == "ToneCurveName")
                .is_some_and(|setting| setting.has_effect && setting.applied)
        );
    }

    #[test]
    fn xmp_develop_exposure_brightens_pixels() {
        let text = r#"<rdf:RDF><rdf:Description crs:Exposure="1.00" /></rdf:RDF>"#;
        let settings = parse_xmp_develop_settings(text, XmpDevelopSource::Embedded);
        let mut image = DecodedImage {
            pixels: vec![96, 96, 96, 255],
            width: 1,
            height: 1,
        };

        apply_xmp_develop_settings(&mut image, &settings);

        assert!(image.pixels[0] > 96);
        assert_eq!(image.pixels[3], 255);
    }

    #[test]
    fn xmp_develop_brightness_above_legacy_neutral_brightens_pixels() {
        let text = r#"<rdf:RDF><rdf:Description crs:Brightness="65" /></rdf:RDF>"#;
        let settings = parse_xmp_develop_settings(text, XmpDevelopSource::Embedded);
        let mut image = DecodedImage {
            pixels: vec![96, 96, 96, 255],
            width: 1,
            height: 1,
        };

        apply_xmp_develop_settings(&mut image, &settings);

        assert!(image.pixels[0] > 96);
        assert_eq!(image.pixels[3], 255);
    }

    #[test]
    fn xmp_develop_sidecar_wins_over_embedded_packet() {
        let dir = make_test_dir("xmp_sidecar");
        let path = dir.join("photo.dng");
        fs::write(&path, b"not really a dng").unwrap();
        fs::write(
            path.with_extension("xmp"),
            r#"<rdf:RDF><rdf:Description crs:Exposure="2.00" /></rdf:RDF>"#,
        )
        .unwrap();
        let embedded = br#"<rdf:RDF><rdf:Description crs:Exposure="1.00" /></rdf:RDF>"#;

        let settings = read_xmp_develop_settings_for_image(&path, Some(embedded));

        assert_eq!(settings.source, Some(XmpDevelopSource::Sidecar));
        assert_eq!(settings.get("Exposure"), Some("2.00"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn xmp_develop_reads_tiff_xml_packet_tag() {
        let dir = make_test_dir("xmp_tiff");
        let path = dir.join("embedded.dng");
        let xmp = r#"<rdf:RDF><rdf:Description crs:Exposure="1.25" crs:CameraProfile="ACR 3.4" /></rdf:RDF>"#;
        write_minimal_dng_with_xmp(&path, xmp);

        let settings = read_xmp_develop_settings_from_path(&path);

        assert_eq!(settings.source, Some(XmpDevelopSource::Embedded));
        assert_eq!(settings.get("Exposure"), Some("1.25"));
        assert_eq!(settings.get("CameraProfile"), Some("ACR 3.4"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn xmp_develop_vignette_darkens_corners() {
        let text = r#"<rdf:RDF><rdf:Description crs:VignetteAmount="-100" /></rdf:RDF>"#;
        let settings = parse_xmp_develop_settings(text, XmpDevelopSource::Embedded);
        let mut image = DecodedImage {
            pixels: vec![200; 3 * 3 * 4],
            width: 3,
            height: 3,
        };
        for alpha in image.pixels.iter_mut().skip(3).step_by(4) {
            *alpha = 255;
        }

        apply_xmp_develop_settings(&mut image, &settings);

        let corner = image.pixels[0];
        let center = image.pixels[4 * 4];
        assert!(corner < center);
    }

    #[test]
    fn xmp_develop_clarity_boosts_local_detail() {
        let text = r#"<rdf:RDF><rdf:Description crs:Clarity="100" /></rdf:RDF>"#;
        let settings = parse_xmp_develop_settings(text, XmpDevelopSource::Embedded);
        let mut image = DecodedImage {
            pixels: vec![32; 3 * 3 * 4],
            width: 3,
            height: 3,
        };
        for alpha in image.pixels.iter_mut().skip(3).step_by(4) {
            *alpha = 255;
        }
        let center = 4 * 4;
        image.pixels[center] = 160;
        image.pixels[center + 1] = 160;
        image.pixels[center + 2] = 160;

        apply_xmp_develop_settings(&mut image, &settings);

        assert!(image.pixels[center] > 160);
    }

    fn write_minimal_dng_with_xmp(path: &std::path::Path, xmp: &str) {
        let xmp_offset = 64usize;
        let mut data = vec![0u8; xmp_offset + xmp.len()];
        data[0..2].copy_from_slice(b"II");
        put_u16_le(&mut data, 2, 42);
        put_u32_le(&mut data, 4, 8);
        put_u16_le(&mut data, 8, 1);
        write_ifd_entry(
            &mut data,
            10,
            0x02bc,
            1,
            xmp.len() as u32,
            xmp_offset as u32,
        );
        put_u32_le(&mut data, 22, 0);
        data[xmp_offset..xmp_offset + xmp.len()].copy_from_slice(xmp.as_bytes());
        fs::write(path, data).unwrap();
    }

    fn write_ifd_entry(
        data: &mut [u8],
        offset: usize,
        tag: u16,
        field_type: u16,
        count: u32,
        value_or_offset: u32,
    ) {
        put_u16_le(data, offset, tag);
        put_u16_le(data, offset + 2, field_type);
        put_u32_le(data, offset + 4, count);
        put_u32_le(data, offset + 8, value_or_offset);
    }

    fn put_u16_le(data: &mut [u8], offset: usize, value: u16) {
        data[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u32_le(data: &mut [u8], offset: usize, value: u32) {
        data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
}
