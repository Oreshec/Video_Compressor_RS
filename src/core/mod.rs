//! Публичная точка входа ядра сжатия.

pub mod bitrate;
pub mod encode;
pub mod ffmpeg;
pub mod presets;
pub mod probe;

pub use bitrate::{CompressionPlan, MediaInfo};
pub use encode::{
    compress, default_output_path, format_duration, CompressionResult, CoreError,
};
pub use ffmpeg::{find_ffmpeg, usable_encoders, FfmpegPaths};
#[allow(unused_imports)]
pub use probe::{probe, ProbeError};
