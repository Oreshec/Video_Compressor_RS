//! Ядро сжатия видео — без UI-зависимостей (SPEC v0.2, §2.2).
//!
//! Компоненты: `MediaProbe` (анализ файлов), `EncodingPlanner` (расчёты
//! планов), `Encoder` (прогоны FFmpeg), `HardwareDetector` (детект кодеров).
//! Внутренняя арифметика — байтовая: целевые размеры хранятся как `u64`
//! байтов, перевод МБ ↔ байты делается на границе UI.

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
