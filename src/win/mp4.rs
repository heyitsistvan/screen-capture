//! The MP4 writer on Windows: Media Foundation's sink writer with the H.264
//! encoder built into Windows (the GPU's, where it has one). Nothing is
//! bundled, so the executable stays small.
//!
//! Media Foundation's DLLs are loaded when a recording starts rather than
//! linked: Windows "N" editions don't have them (until the Media Feature Pack
//! is installed), and the app must still start there.

use std::ffi::c_void;
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::OnceLock;

use windows::core::{Interface, GUID, HRESULT, PCWSTR};
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_Quality, eAVEncH264VProfile_High, IMFAttributes, IMFByteStream, IMFMediaBuffer,
    IMFMediaType, IMFSample, IMFSinkWriter, MF_ACCESSMODE_READWRITE, MF_FILEFLAGS_NONE, MF_OPENMODE_DELETE_IF_EXIST, MFMediaType_Video, MFNominalRange_16_235, MFTranscodeContainerType_MPEG4,
    MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFVideoPrimaries_BT709,
    MFVideoTransFunc_709, MFVideoTransferMatrix_BT709, CODECAPI_AVEncCommonQuality,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVGOPSize, MFSTARTUP_LITE,
    MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION,
    MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
    MF_TRANSCODE_CONTAINERTYPE, MF_VERSION,
};
use windows_sys::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};

use crate::mp4_enc::VideoWriter;
use crate::util::{wide, wide_path};

type StartupFn = unsafe extern "system" fn(u32, u32) -> HRESULT;
type ShutdownFn = unsafe extern "system" fn() -> HRESULT;
type AttributesFn = unsafe extern "system" fn(*mut *mut c_void, u32) -> HRESULT;
/// Creates an object, returned through the out pointer.
type OutFn = unsafe extern "system" fn(*mut *mut c_void) -> HRESULT;
type BufferFn = unsafe extern "system" fn(u32, *mut *mut c_void) -> HRESULT;
type FileFn = unsafe extern "system" fn(i32, i32, i32, PCWSTR, *mut *mut c_void) -> HRESULT;
type SinkWriterFn = unsafe extern "system" fn(PCWSTR, *mut c_void, *mut c_void, *mut *mut c_void) -> HRESULT;

/// The Media Foundation functions used (the rest are COM methods).
struct Api {
    startup: StartupFn,
    shutdown: ShutdownFn,
    create_attributes: AttributesFn,
    create_media_type: OutFn,
    create_sample: OutFn,
    create_memory_buffer: BufferFn,
    create_file: FileFn,
    create_sink_writer: SinkWriterFn,
}

fn api() -> Result<&'static Api, String> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| unsafe { load() }).as_ref().ok_or_else(|| {
        "Recording MP4 needs Windows' Media Foundation, which this edition of Windows doesn't include \
         (install the Media Feature Pack). GIF recording still works."
            .into()
    })
}

unsafe fn load() -> Option<Api> {
    let lib = |name: &str| {
        let w = wide(name);
        let h = LoadLibraryExW(w.as_ptr(), null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32);
        (!h.is_null()).then_some(h)
    };
    let (plat, rw) = (lib("mfplat.dll")?, lib("mfreadwrite.dll")?);
    // Each export, given the signature it's documented with.
    macro_rules! get {
        ($m:expr, $name:literal, $t:ty) => {
            std::mem::transmute::<unsafe extern "system" fn() -> isize, $t>(GetProcAddress(
                $m,
                concat!($name, "\0").as_ptr(),
            )?)
        };
    }
    Some(Api {
        startup: get!(plat, "MFStartup", StartupFn),
        shutdown: get!(plat, "MFShutdown", ShutdownFn),
        create_attributes: get!(plat, "MFCreateAttributes", AttributesFn),
        create_media_type: get!(plat, "MFCreateMediaType", OutFn),
        create_sample: get!(plat, "MFCreateSample", OutFn),
        create_memory_buffer: get!(plat, "MFCreateMemoryBuffer", BufferFn),
        create_file: get!(plat, "MFCreateFile", FileFn),
        create_sink_writer: get!(rw, "MFCreateSinkWriterFromURL", SinkWriterFn),
    })
}

/// Calls a function returning a new COM object through an out pointer.
unsafe fn make<T: Interface>(what: &str, f: impl FnOnce(*mut *mut c_void) -> HRESULT) -> Result<T, String> {
    let mut p = null_mut();
    f(&mut p).ok().map_err(|e| err(what, e))?;
    if p.is_null() {
        return Err(format!("{what}: no object"));
    }
    Ok(T::from_raw(p))
}

fn err(what: &str, e: windows::core::Error) -> String {
    let msg = e.message();
    if msg.is_empty() {
        format!("{what} failed ({:#010x})", e.code().0)
    } else {
        format!("{what} failed: {msg}")
    }
}

/// Lets a `windows` call's error carry what was being done.
trait Context<T> {
    fn ctx(self, what: &str) -> Result<T, String>;
}

impl<T> Context<T> for windows::core::Result<T> {
    fn ctx(self, what: &str) -> Result<T, String> {
        self.map_err(|e| err(what, e))
    }
}

pub struct Writer {
    api: &'static Api,
    /// Released before Media Foundation and COM shut down.
    sink: Option<IMFSinkWriter>,
    /// The file, closed explicitly so it can be renamed or deleted at once.
    file: Option<IMFByteStream>,
    stream: u32,
    len: usize,
    mf_started: bool,
    com_initialized: bool,
}

impl VideoWriter for Writer {
    const NV12: bool = true;
    /// What Windows' own (software) H.264 encoder takes; the GPU encoders
    /// tried take the same.
    const MIN_SIZE: i32 = 34;
    const MAX_SIZE: (i32, i32) = (4096, 2304);

    fn create(path: &Path, w: i32, h: i32, fps: u32) -> Result<Writer, String> {
        let api = api()?;
        let len = w as usize * h as usize * 3 / 2;
        let mut writer = Writer { api, sink: None, file: None, stream: 0, len, mf_started: false, com_initialized: false };
        unsafe {
            // The encoder runs on its own thread; Media Foundation wants COM.
            writer.com_initialized = CoInitializeEx(null(), COINIT_MULTITHREADED as u32) >= 0;
            (api.startup)(MF_VERSION, MFSTARTUP_LITE).ok().ctx("Starting Media Foundation")?;
            writer.mf_started = true;
            // A GPU encoder where there is one; Windows' own if that fails.
            let mut attempt = |hardware: bool| -> Result<(), String> {
                writer.close();
                // A stream of our own rather than a file name, so it can be
                // closed (and the file renamed or deleted) right away.
                let url = wide_path(path);
                let file: IMFByteStream = make("Creating the MP4 file", |p| {
                    (api.create_file)(MF_ACCESSMODE_READWRITE.0, MF_OPENMODE_DELETE_IF_EXIST.0, MF_FILEFLAGS_NONE.0, PCWSTR(url.as_ptr()), p)
                })?;
                writer.file = Some(file.clone());
                let (sink, stream) = open(api, &file, w, h, fps, hardware)?;
                writer.sink = Some(sink);
                writer.stream = stream;
                Ok(())
            };
            if let Err(gpu) = attempt(true) {
                attempt(false).map_err(|e| format!("{e} (GPU encoder: {gpu})"))?;
            }
        }
        Ok(writer)
    }

    fn write(&mut self, yuv: &[u8], time_ms: u64, duration_ms: u64) -> Result<(), String> {
        let Some(sink) = &self.sink else { return Err("The MP4 writer is closed".into()) };
        let len = self.len.min(yuv.len());
        unsafe {
            let buf: IMFMediaBuffer = make("Allocating a frame", |p| (self.api.create_memory_buffer)(len as u32, p))?;
            let mut data = null_mut();
            buf.Lock(&mut data, None, None).ctx("Locking a frame")?;
            std::ptr::copy_nonoverlapping(yuv.as_ptr(), data, len);
            buf.Unlock().ctx("Unlocking a frame")?;
            buf.SetCurrentLength(len as u32).ctx("Sizing a frame")?;
            let sample: IMFSample = make("Creating a sample", |p| (self.api.create_sample)(p))?;
            sample.AddBuffer(&buf).ctx("Adding a frame")?;
            // Media Foundation counts in 100 ns units.
            sample.SetSampleTime(time_ms as i64 * 10_000).ctx("Timing a frame")?;
            sample.SetSampleDuration(duration_ms as i64 * 10_000).ctx("Timing a frame")?;
            sink.WriteSample(self.stream, &sample).ctx("Encoding a frame")
        }
    }

    fn finish(mut self) -> Result<(), String> {
        let sink = self.sink.take().ok_or("The MP4 writer is closed")?;
        unsafe { sink.Finalize() }.ctx("Finishing the MP4 file")?;
        drop(sink);
        self.close();
        Ok(())
    }
}

impl Writer {
    /// Lets go of the encoder and closes the file.
    fn close(&mut self) {
        self.sink = None;
        if let Some(f) = self.file.take() {
            let _ = unsafe { f.Close() };
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // Closing the file lets a cancelled one be deleted; then Media
        // Foundation and COM can go.
        self.close();
        unsafe {
            if self.mf_started {
                let _ = (self.api.shutdown)();
            }
            if self.com_initialized {
                CoUninitialize();
            }
        }
    }
}

/// Creates the sink writer with an H.264 stream fed NV12 frames.
unsafe fn open(api: &Api, file: &IMFByteStream, w: i32, h: i32, fps: u32, hardware: bool) -> Result<(IMFSinkWriter, u32), String> {
    let attrs: IMFAttributes = make("Creating attributes", |p| (api.create_attributes)(p, 3))?;
    // Written to a stream rather than a named file, so name the container.
    attrs.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4).ctx("Choosing MP4")?;
    attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, hardware as u32).ctx("Choosing the encoder")?;
    // Not MF_MPEG4SINK_MOOV_BEFORE_MDAT: the files it rearranges have broken
    // chunk offsets. The shared faststart moves the index instead.
    let sink: IMFSinkWriter = make("Starting the MP4 writer", |p| {
        (api.create_sink_writer)(PCWSTR(null()), file.as_raw(), attrs.as_raw(), p)
    })?;

    let out = video_type(api, &MFVideoFormat_H264, w, h, fps)?;
    out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate(w, h, fps)).ctx("Setting the bitrate")?;
    out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32).ctx("Setting the profile")?;
    let stream = sink.AddStream(&out).ctx("Adding the video stream")?;

    let input = video_type(api, &MFVideoFormat_NV12, w, h, fps)?;
    // Top-down rows.
    input.SetUINT32(&MF_MT_DEFAULT_STRIDE, w as u32).ctx("Setting the stride")?;
    // Constant quality: static screens cost almost nothing, motion gets the
    // bits it needs. A keyframe every two seconds or so, for seeking.
    let params: IMFAttributes = make("Creating encoder settings", |p| (api.create_attributes)(p, 3))?;
    params.SetUINT32(&CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_Quality.0 as u32).ctx("Encoder settings")?;
    params.SetUINT32(&CODECAPI_AVEncCommonQuality, 70).ctx("Encoder settings")?;
    params.SetUINT32(&CODECAPI_AVEncMPVGOPSize, fps * 2).ctx("Encoder settings")?;
    sink.SetInputMediaType(stream, &input, &params).ctx("Starting the H.264 encoder")?;
    sink.BeginWriting().ctx("Starting the MP4 file")?;
    Ok((sink, stream))
}

unsafe fn video_type(api: &Api, subtype: &GUID, w: i32, h: i32, fps: u32) -> Result<IMFMediaType, String> {
    let t: IMFMediaType = make("Creating a media type", |p| (api.create_media_type)(p))?;
    let pair = |a: u32, b: u32| (a as u64) << 32 | b as u64;
    t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ctx("Media type")?;
    t.SetGUID(&MF_MT_SUBTYPE, subtype).ctx("Media type")?;
    t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).ctx("Media type")?;
    t.SetUINT64(&MF_MT_FRAME_SIZE, pair(w as u32, h as u32)).ctx("Media type")?;
    t.SetUINT64(&MF_MT_FRAME_RATE, pair(fps, 1)).ctx("Media type")?;
    t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pair(1, 1)).ctx("Media type")?;
    // The colors as converted (BT.709, limited range), so players show them
    // as captured.
    t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32).ctx("Media type")?;
    t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32).ctx("Media type")?;
    t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32).ctx("Media type")?;
    t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32).ctx("Media type")?;
    Ok(t)
}

/// The bitrate the encoder is told to aim for (it's a ceiling for the
/// constant-quality mode): enough for sharp text in motion.
pub fn bitrate(w: i32, h: i32, fps: u32) -> u32 {
    let bits = w as f64 * h as f64 * fps as f64 * 0.15;
    bits.clamp(500_000.0, 40_000_000.0) as u32
}
