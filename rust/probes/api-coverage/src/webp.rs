//! webp-animation (libwebp's `WebPAnimEncoder`) covering `synthesize_ugoira_webp`
//! in `src/web/pixiv.py`: Pillow `save(save_all=True, duration=[...], loop=0,
//! quality=90, method=4)` over JPEG or PNG frames.

use webp_animation::{AnimParams, ColorMode, Encoder, EncoderOptions, EncodingConfig, EncodingType, LossyEncodingConfig};

/// Pillow's knobs as `src/web/pixiv.py` sets them.
pub const QUALITY: f32 = 90.0;
pub const METHOD: usize = 4;

/// Encode RGBA frames with per-frame delays into one looping animated webp.
pub fn encode(width: u32, height: u32, frames: &[Vec<u8>], delays_ms: &[i32]) -> Result<Vec<u8>, webp_animation::Error> {
    let options = EncoderOptions {
        anim_params: AnimParams { loop_count: 0 },
        color_mode: ColorMode::Rgba,
        encoding_config: Some(EncodingConfig {
            encoding_type: EncodingType::Lossy(LossyEncodingConfig::default()),
            quality: QUALITY,
            method: METHOD,
        }),
        ..Default::default()
    };
    let mut encoder = Encoder::new_with_options((width, height), options)?;
    let mut timestamp = 0_i32;
    for (frame, delay) in frames.iter().zip(delays_ms) {
        encoder.add_frame(frame, timestamp)?;
        timestamp += delay;
    }
    Ok(encoder.finalize(timestamp)?.to_vec())
}

/// Decode JPEG or PNG frame bytes the way Pillow opens them, as RGBA.
pub fn decode_frame(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), image::ImageError> {
    let rgba = image::load_from_memory(bytes)?.to_rgba8();
    Ok((rgba.width(), rgba.height(), rgba.into_raw()))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use webp_animation::Decoder;

    use super::*;

    fn encoded(format: image::ImageFormat, shade: u8) -> Vec<u8> {
        let frame = image::RgbImage::from_pixel(16, 8, image::Rgb([shade, 255 - shade, 64]));
        let mut out = Cursor::new(Vec::new());
        frame.write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    #[test]
    fn ugoira_frames_become_one_looping_animation_with_their_delays() {
        let sources = [
            encoded(image::ImageFormat::Jpeg, 10),
            encoded(image::ImageFormat::Png, 120),
            encoded(image::ImageFormat::Jpeg, 240),
        ];
        let frames: Vec<Vec<u8>> = sources.iter().map(|bytes| decode_frame(bytes).unwrap().2).collect();
        let webp = encode(16, 8, &frames, &[100, 250, 80]).unwrap();

        assert_eq!(&webp[..4], b"RIFF");
        assert_eq!(&webp[8..12], b"WEBP");
        let decoder = Decoder::new(&webp).unwrap();
        assert_eq!(decoder.dimensions(), (16, 8));
        let timestamps: Vec<i32> = decoder.into_iter().map(|frame| frame.timestamp()).collect();
        // Each timestamp is the frame's end time: 100, 100 + 250, 100 + 250 + 80.
        assert_eq!(timestamps, vec![100, 350, 430]);
    }
}
