//! Spike 4 — anneau de segments disque et sauvegarde instantanée. **Risque R3.**
//!
//! Questions posées :
//!
//! 1. Peut-on écrire en continu des segments courts sans à-coup à la rotation ?
//! 2. L'anneau se purge-t-il correctement, borné **en durée et en octets** ?
//!    Le Spike 1 a montré que le MFT AMD ignore tout plafond de débit : une
//!    borne en durée seule ne protège pas le disque.
//! 3. La sauvegarde est-elle instantanée ? Critère : **< 1 s** pour concaténer
//!    l'anneau, sans réencodage.
//!
//! Le point délicat est la concaténation. Elle se fait en *passthrough* Media
//! Foundation : un `IMFSourceReader` auquel on ne configure aucun décodeur rend
//! les échantillons H.264 et AAC compressés tels quels, qu'un `IMFSinkWriter`
//! réécrit sans les toucher. Aucun appel à ffmpeg, aucun réencodage.
//!
//! Chaque segment est un MP4 autonome démarrant à PTS 0 : comme un nouveau
//! `SinkWriter` émet toujours une IDR en première frame, la frontière de
//! segment est une frontière de GOP par construction.
//!
//! Usage : `cargo run --release --bin spike4_ring -- --minutes 2 --buffer 30`

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
    ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{HMONITOR, MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint};
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, IAudioSessionControl2, IAudioSessionManager2, IMMDevice,
    IMMDeviceEnumerator, MMDeviceEnumerator, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
    VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX, eCapture, eConsole, eRender,
};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFAttributes, IMFDXGIDeviceManager, IMFMediaType, IMFSample, IMFSinkWriter,
    MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE,
    MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND,
    MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
    MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SINK_WRITER_D3D_MANAGER,
    MF_SINK_WRITER_DISABLE_THROTTLING, MF_SOURCE_READER_ANY_STREAM, MF_SOURCE_READERF_ENDOFSTREAM,
    MF_VERSION, MFAudioFormat_AAC, MFAudioFormat_PCM, MFCreateAttributes,
    MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFCreateSinkWriterFromURL, MFCreateSourceReaderFromURL, MFMediaType_Audio,
    MFMediaType_Video, MFSTARTUP_FULL, MFShutdown, MFStartup, MFVideoFormat_ARGB32,
    MFVideoFormat_H264, MFVideoInterlace_Progressive, eAVEncH264VProfile_High,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{GUID, HSTRING, Interface, PCWSTR, Ref, implement};

use smartclip_core::clock::{HNS_PER_SEC, MasterClock, QpcInstant};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const TEXTURE_RING: usize = 8;

fn hns_since_boot(ticks: i64, freq: i64) -> i64 {
    ((ticks as i128 * HNS_PER_SEC as i128) / freq as i128) as i64
}

// ───────────────────────────── capture audio (Spike 2/3) ──────────────────────

struct AudioChunk {
    track: usize,
    pcm: Vec<i16>,
    boot_hns: i64,
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationHandler {
    done: HANDLE,
}
unsafe impl Send for ActivationHandler {}
unsafe impl Sync for ActivationHandler {}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationHandler_Impl {
    fn ActivateCompleted(
        &self,
        _op: Ref<'_, IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        unsafe { windows::Win32::System::Threading::SetEvent(self.done) }
    }
}

fn activate_process_loopback(pid: u32) -> Result<IAudioClient> {
    unsafe {
        // Sur le tas et fuité : le service audio relit la structure après la fin
        // de l'activation. Sur la pile → STATUS_HEAP_CORRUPTION (vu au Spike 2).
        let params = Box::leak(Box::new(AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        }));
        let mut variant = PROPVARIANT::default();
        {
            let inner = &mut variant.Anonymous.Anonymous;
            inner.vt = VT_BLOB;
            inner.Anonymous.blob.cbSize =
                std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32;
            inner.Anonymous.blob.pBlobData = (params as *mut AUDIOCLIENT_ACTIVATION_PARAMS).cast();
        }
        let done = CreateEventW(None, false, false, PCWSTR::null())?;
        let handler: IActivateAudioInterfaceCompletionHandler = ActivationHandler { done }.into();
        let op = ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&variant),
            &handler,
        )?;
        let waited = WaitForSingleObject(done, 5_000);
        let _ = CloseHandle(done);
        if waited != WAIT_OBJECT_0 {
            bail!("activation expirée pour le pid {pid}");
        }
        let mut hr = windows::core::HRESULT(0);
        let mut unknown = None;
        op.GetActivateResult(&mut hr, &mut unknown)?;
        hr.ok()?;
        Ok(unknown.context("pas d'interface")?.cast()?)
    }
}

fn activate_microphone() -> Result<IAudioClient> {
    unsafe {
        let e: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let d: IMMDevice = e.GetDefaultAudioEndpoint(eCapture, eConsole)?;
        Ok(d.Activate(CLSCTX_ALL, None)?)
    }
}

fn discover_sources(max: usize) -> Result<Vec<u32>> {
    unsafe {
        let e: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let d: IMMDevice = e.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let m: IAudioSessionManager2 = d.Activate(CLSCTX_ALL, None)?;
        let sessions = m.GetSessionEnumerator()?;
        let mut pids = Vec::new();
        for i in 0..sessions.GetCount()? {
            if pids.len() >= max {
                break;
            }
            let Ok(c) = sessions.GetSession(i)?.cast::<IAudioSessionControl2>() else {
                continue;
            };
            let pid = c.GetProcessId()?;
            if pid != 0 && !pids.contains(&pid) {
                pids.push(pid);
            }
        }
        Ok(pids)
    }
}

fn audio_thread(
    track: usize,
    pid: Option<u32>,
    clock: MasterClock,
    stop: Arc<AtomicBool>,
    tx: Sender<AudioChunk>,
) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let run = (|| -> Result<()> {
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
            nChannels: CHANNELS,
            nSamplesPerSec: SAMPLE_RATE,
            nAvgBytesPerSec: SAMPLE_RATE * CHANNELS as u32 * 4,
            nBlockAlign: CHANNELS * 4,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        let (client, loopback) = match pid {
            Some(p) => (activate_process_loopback(p)?, true),
            None => (activate_microphone()?, false),
        };
        let mut flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
        if loopback {
            flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
        }
        unsafe {
            client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 2_000_000, 0, &format, None)?;
            let event = CreateEventW(None, false, false, PCWSTR::null())?;
            client.SetEventHandle(event)?;
            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            while !stop.load(Ordering::Relaxed) {
                if WaitForSingleObject(event, 200) != WAIT_OBJECT_0 {
                    continue;
                }
                while capture.GetNextPacketSize()? > 0 {
                    let mut data = std::ptr::null_mut();
                    let (mut frames, mut pf, mut qpc) = (0u32, 0u32, 0u64);
                    capture.GetBuffer(&mut data, &mut frames, &mut pf, None, Some(&mut qpc))?;
                    let count = frames as usize * CHANNELS as usize;
                    let pcm: Vec<i16> = if pf & 0x2 != 0 || data.is_null() {
                        vec![0; count]
                    } else {
                        std::slice::from_raw_parts(data as *const f32, count)
                            .iter()
                            .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
                            .collect()
                    };
                    capture.ReleaseBuffer(frames)?;
                    let boot_hns = hns_since_boot(QpcInstant::from_u64(qpc).0, clock.frequency());
                    if tx.send(AudioChunk { track, pcm, boot_hns }).is_err() {
                        return Ok(());
                    }
                }
            }
            client.Stop()?;
        }
        Ok(())
    })();
    if let Err(e) = run {
        tracing::error!("piste audio {track} : {e:#}");
    }
}

// ─────────────────────────────── écriture d'un segment ────────────────────────

/// Un segment MP4 autonome : vidéo + N pistes audio, PTS repartant de zéro.
struct Segment {
    writer: IMFSinkWriter,
    video_stream: u32,
    audio_streams: Vec<u32>,
    path: PathBuf,
    /// PTS du dernier échantillon écrit, qui sert de durée à la fermeture.
    last_pts_hns: i64,
}

fn pack(t: &IMFMediaType, key: &GUID, hi: u32, lo: u32) -> Result<()> {
    unsafe { t.SetUINT64(key, ((hi as u64) << 32) | lo as u64)? };
    Ok(())
}

fn open_segment(
    manager: &IMFDXGIDeviceManager,
    path: PathBuf,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    audio_tracks: usize,
) -> Result<Segment> {
    let mut attributes: Option<IMFAttributes> = None;
    unsafe { MFCreateAttributes(&mut attributes, 3)? };
    let attributes = attributes.context("attributs nuls")?;
    unsafe {
        attributes.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
        attributes.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
        attributes.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, manager)?;
    }
    let writer = unsafe {
        MFCreateSinkWriterFromURL(&HSTRING::from(path.to_string_lossy().as_ref()), None, &attributes)?
    };

    let out = unsafe { MFCreateMediaType()? };
    unsafe {
        out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
        out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
    }
    pack(&out, &MF_MT_FRAME_SIZE, width, height)?;
    pack(&out, &MF_MT_FRAME_RATE, fps, 1)?;
    pack(&out, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
    let video_stream = unsafe { writer.AddStream(&out)? };

    let inp = unsafe { MFCreateMediaType()? };
    unsafe {
        inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_ARGB32)?;
        inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    }
    pack(&inp, &MF_MT_FRAME_SIZE, width, height)?;
    pack(&inp, &MF_MT_FRAME_RATE, fps, 1)?;
    pack(&inp, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
    unsafe { writer.SetInputMediaType(video_stream, &inp, None)? };

    let mut audio_streams = Vec::with_capacity(audio_tracks);
    for _ in 0..audio_tracks {
        let out = unsafe { MFCreateMediaType()? };
        unsafe {
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
            out.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            out.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)?;
            out.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS as u32)?;
            out.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 24_000)?;
        }
        let s = unsafe { writer.AddStream(&out)? };
        let inp = unsafe { MFCreateMediaType()? };
        unsafe {
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            inp.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            inp.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)?;
            inp.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS as u32)?;
            inp.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, (CHANNELS * 2) as u32)?;
            writer.SetInputMediaType(s, &inp, None)?;
        }
        audio_streams.push(s);
    }

    unsafe { writer.BeginWriting()? };
    Ok(Segment {
        writer,
        video_stream,
        audio_streams,
        path,
        last_pts_hns: 0,
    })
}

impl Segment {
    fn write_video(&mut self, texture: &ID3D11Texture2D, pts: i64, duration: i64) -> Result<()> {
        unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)?;
            let len = buffer.cast::<IMF2DBuffer>()?.GetContiguousLength()?;
            buffer.SetCurrentLength(len)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(pts)?;
            sample.SetSampleDuration(duration)?;
            self.writer.WriteSample(self.video_stream, &sample)?;
        }
        self.last_pts_hns = self.last_pts_hns.max(pts + duration);
        Ok(())
    }

    fn write_audio(&mut self, track: usize, pcm: &[i16], pts: i64) -> Result<()> {
        let Some(&stream) = self.audio_streams.get(track) else {
            return Ok(());
        };
        let frames = pcm.len() / CHANNELS as usize;
        let duration = frames as i64 * HNS_PER_SEC / SAMPLE_RATE as i64;
        unsafe {
            let bytes = std::mem::size_of_val(pcm);
            let buffer = MFCreateMemoryBuffer(bytes as u32)?;
            let mut dst = std::ptr::null_mut();
            buffer.Lock(&mut dst, None, None)?;
            std::ptr::copy_nonoverlapping(pcm.as_ptr() as *const u8, dst, bytes);
            buffer.Unlock()?;
            buffer.SetCurrentLength(bytes as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(pts)?;
            sample.SetSampleDuration(duration)?;
            self.writer.WriteSample(stream, &sample)?;
        }
        self.last_pts_hns = self.last_pts_hns.max(pts + duration);
        Ok(())
    }

    /// Ferme le segment et renvoie sa description.
    ///
    /// C'est cet appel qui doit être déclenché immédiatement à l'appui du
    /// raccourci : sans lui, le segment en cours n'est pas lisible et l'on perd
    /// jusqu'à sa durée entière — c'est-à-dire justement l'instant à sauver.
    fn close(self) -> Result<SegmentInfo> {
        unsafe { self.writer.Finalize()? };
        let bytes = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        Ok(SegmentInfo {
            path: self.path,
            bytes,
            duration_hns: self.last_pts_hns,
        })
    }
}

/// Transport d'un segment entre threads.
///
/// Les interfaces COM ne sont pas `Send` en Rust, mais tous les threads de ce
/// spike sont en MTA : un objet y est soit agile, soit accédé via un proxy que
/// COM fournit lui-même. Le `SinkWriter` n'est de plus jamais touché par deux
/// threads à la fois — il est transféré, pas partagé.
struct SendSegment(Segment);
unsafe impl Send for SendSegment {}

struct SendManager(IMFDXGIDeviceManager);
unsafe impl Send for SendManager {}

impl SendManager {
    /// Accesseur volontaire plutôt qu'un `.0` direct : la capture disjointe de
    /// Rust 2021 prendrait le champ COM nu, qui n'est pas `Send`, au lieu du
    /// wrapper. Passer par une méthode force la capture de la structure entière.
    fn get(&self) -> &IMFDXGIDeviceManager {
        &self.0
    }
}

#[derive(Debug, Clone)]
struct SegmentInfo {
    path: PathBuf,
    bytes: u64,
    duration_hns: i64,
}

// ──────────────────────────────── anneau borné ────────────────────────────────

/// Anneau de segments borné **à la fois** en durée et en octets.
///
/// La double borne n'est pas une précaution de principe : le Spike 1 a montré
/// que le MFT AMD ignore `MF_MT_AVG_BITRATE` comme `AVEncCommonMaxBitRate`, et
/// produit jusqu'au double de la consigne. Une borne en durée seule laisserait
/// donc le disque se remplir sans plafond connu.
struct SegmentRing {
    segments: Vec<SegmentInfo>,
    max_duration_hns: i64,
    max_bytes: u64,
    /// Segments purgés depuis le début, pour distinguer une purge de durée
    /// d'une purge de budget.
    evicted_by_duration: usize,
    evicted_by_bytes: usize,
}

impl SegmentRing {
    fn new(max_seconds: f64, max_bytes: u64) -> Self {
        Self {
            segments: Vec::new(),
            max_duration_hns: (max_seconds * HNS_PER_SEC as f64) as i64,
            max_bytes,
            evicted_by_duration: 0,
            evicted_by_bytes: 0,
        }
    }

    fn total_hns(&self) -> i64 {
        self.segments.iter().map(|s| s.duration_hns).sum()
    }

    fn total_bytes(&self) -> u64 {
        self.segments.iter().map(|s| s.bytes).sum()
    }

    fn push(&mut self, segment: SegmentInfo) {
        self.segments.push(segment);
        // On purge tant que l'une OU l'autre borne est dépassée. Le segment le
        // plus ancien part en premier : c'est le moins susceptible d'intéresser.
        while self.segments.len() > 1 {
            let over_duration = self.total_hns() > self.max_duration_hns;
            let over_bytes = self.total_bytes() > self.max_bytes;
            if !over_duration && !over_bytes {
                break;
            }
            if over_duration {
                self.evicted_by_duration += 1;
            } else {
                self.evicted_by_bytes += 1;
            }
            let old = self.segments.remove(0);
            let _ = std::fs::remove_file(&old.path);
        }
    }
}

// ─────────────────────── concaténation sans réencodage ────────────────────────

/// Concatène les segments en recopiant les échantillons compressés.
///
/// Le `IMFSourceReader` est créé sans activer le moindre décodeur : les types
/// natifs restent H.264 et AAC, et `ReadSample` rend les échantillons encore
/// compressés. Le `IMFSinkWriter` déclare exactement ces mêmes types en entrée
/// comme en sortie, ce qui le met en simple recopie. Aucun pixel n'est touché.
fn concat_passthrough(segments: &[SegmentInfo], out: &Path) -> Result<(u64, usize)> {
    if segments.is_empty() {
        bail!("aucun segment à concaténer");
    }

    let mut attributes: Option<IMFAttributes> = None;
    unsafe { MFCreateAttributes(&mut attributes, 1)? };
    let attributes = attributes.context("attributs nuls")?;

    let writer = unsafe {
        MFCreateSinkWriterFromURL(&HSTRING::from(out.to_string_lossy().as_ref()), None, None)?
    };

    // Le premier segment fixe la structure du fichier de sortie : un flux de
    // sortie par flux source, aux types natifs.
    let first = unsafe {
        MFCreateSourceReaderFromURL(
            &HSTRING::from(segments[0].path.to_string_lossy().as_ref()),
            &attributes,
        )?
    };
    let mut stream_map: Vec<(u32, u32)> = Vec::new(); // (index source, index writer)
    let mut index = 0u32;
    loop {
        let native = match unsafe { first.GetCurrentMediaType(index) } {
            Ok(t) => t,
            Err(_) => break,
        };
        unsafe {
            first.SetStreamSelection(index, true)?;
            let writer_stream = writer.AddStream(&native)?;
            writer.SetInputMediaType(writer_stream, &native, None)?;
            stream_map.push((index, writer_stream));
        }
        index += 1;
    }
    drop(first);

    unsafe { writer.BeginWriting()? };

    let mut offset_hns = 0i64;
    let mut samples = 0usize;

    for segment in segments {
        let reader = unsafe {
            MFCreateSourceReaderFromURL(
                &HSTRING::from(segment.path.to_string_lossy().as_ref()),
                &attributes,
            )?
        };
        for (source_stream, _) in &stream_map {
            unsafe { reader.SetStreamSelection(*source_stream, true)? };
        }

        loop {
            let mut actual = 0u32;
            let mut flags = 0u32;
            let mut timestamp = 0i64;
            let mut sample: Option<IMFSample> = None;
            unsafe {
                reader.ReadSample(
                    MF_SOURCE_READER_ANY_STREAM.0 as u32,
                    0,
                    Some(&mut actual),
                    Some(&mut flags),
                    Some(&mut timestamp),
                    Some(&mut sample),
                )?;
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 && sample.is_none() {
                break;
            }
            let Some(sample) = sample else { continue };
            let Some((_, writer_stream)) =
                stream_map.iter().find(|(src, _)| *src == actual)
            else {
                continue;
            };
            unsafe {
                // Chaque segment repart de zéro : on le replace sur la timeline
                // globale en décalant de la durée cumulée des précédents.
                sample.SetSampleTime(timestamp + offset_hns)?;
                writer.WriteSample(*writer_stream, &sample)?;
            }
            samples += 1;
        }
        offset_hns += segment.duration_hns;
    }

    unsafe { writer.Finalize()? };
    let bytes = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
    Ok((bytes, samples))
}

// ─────────────────────────────────── main ─────────────────────────────────────

struct Args {
    minutes: f64,
    buffer_seconds: f64,
    max_mb: u64,
    segment_seconds: f64,
    fps: u32,
    bitrate: u32,
    workdir: PathBuf,
    max_sources: usize,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut a = Args {
            minutes: 1.0,
            buffer_seconds: 30.0,
            max_mb: 1024,
            segment_seconds: 2.0,
            fps: 60,
            bitrate: 20_000_000,
            workdir: std::env::temp_dir().join("smartclip_spike4"),
            max_sources: 3,
        };
        let mut it = std::env::args().skip(1);
        while let Some(flag) = it.next() {
            let mut v = || it.next().with_context(|| format!("valeur manquante après {flag}"));
            match flag.as_str() {
                "--minutes" => a.minutes = v()?.parse()?,
                "--buffer" => a.buffer_seconds = v()?.parse()?,
                "--max-mb" => a.max_mb = v()?.parse()?,
                "--segment" => a.segment_seconds = v()?.parse()?,
                "--fps" => a.fps = v()?.parse()?,
                "--bitrate" => a.bitrate = v()?.parse()?,
                "--workdir" => a.workdir = PathBuf::from(v()?),
                "--max-sources" => a.max_sources = v()?.parse()?,
                other => bail!("option inconnue : {other}"),
            }
        }
        Ok(a)
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse()?;
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
    }
    let r = run(&args);
    unsafe {
        let _ = MFShutdown();
        CoUninitialize();
    }
    r
}

fn run(args: &Args) -> Result<()> {
    let _ = std::fs::remove_dir_all(&args.workdir);
    std::fs::create_dir_all(&args.workdir)?;

    let (device, context) = create_d3d_device()?;
    let _ = unsafe { device.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true) };

    let monitor: HMONITOR = unsafe { MonitorFromPoint(Default::default(), MONITOR_DEFAULTTOPRIMARY) };
    let interop = windows::core::factory::<GraphicsCaptureItem, windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop>()?;
    let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(monitor)? };
    let size = item.Size()?;
    let (width, height) = (size.Width as u32, size.Height as u32);

    let dxgi: IDXGIDevice = device.cast()?;
    let winrt_device: IDirect3DDevice = unsafe {
        windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?
    }
    .cast()?;
    let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &winrt_device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        size,
    )?;
    let session = frame_pool.CreateCaptureSession(&item)?;
    let _ = session.SetIsBorderRequired(false);

    let mut token = 0u32;
    let mut manager: Option<IMFDXGIDeviceManager> = None;
    unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager)? };
    let manager = manager.context("device manager nul")?;
    unsafe { manager.ResetDevice(&device, token)? };

    let pids = discover_sources(args.max_sources)?;
    let audio_tracks = pids.len() + 1; // + le micro
    tracing::info!(
        "{width}×{height}@{} — {audio_tracks} piste(s) audio — segments de {}s — \
         anneau {}s / {} Mo",
        args.fps,
        args.segment_seconds,
        args.buffer_seconds,
        args.max_mb
    );

    let clock = MasterClock::new();
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx): (Sender<AudioChunk>, Receiver<AudioChunk>) = channel();
    let mut handles = Vec::new();
    for (track, pid) in pids.iter().map(|p| Some(*p)).chain([None]).enumerate() {
        let (clock, stop, tx) = (clock.clone(), Arc::clone(&stop), tx.clone());
        handles.push(std::thread::spawn(move || {
            audio_thread(track, pid, clock, stop, tx)
        }));
    }
    drop(tx);

    let ring_textures = create_texture_ring(&device, width, height)?;
    session.StartCapture()?;

    let frame_duration = HNS_PER_SEC / args.fps as i64;
    let tick = Duration::from_nanos(1_000_000_000 / args.fps as u64);
    let total_frames = (args.minutes * 60.0 * args.fps as f64).round() as u64;
    let frames_per_segment = (args.segment_seconds * args.fps as f64).round() as u64;

    let mut ring = SegmentRing::new(args.buffer_seconds, args.max_mb * 1_048_576);

    // ── segments préparés et finalisés hors de la boucle de capture ──
    //
    // Le premier essai créait et finalisait le segment dans la boucle : 678 ms
    // de blocage en moyenne toutes les 2 s, pendant lesquelles l'image restait
    // figée. `Finalize` écrit l'index du MP4 et la création d'un SinkWriter
    // réinitialise le MFT matériel — deux opérations bien trop lourdes pour le
    // chemin critique. Déportées sur deux threads, la rotation se réduit à un
    // échange de pointeur.
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<SendSegment>(1);
    let (close_tx, close_rx) = channel::<SendSegment>();
    let (closed_tx, closed_rx) = channel::<SegmentInfo>();

    let opener = {
        let manager = SendManager(manager.clone());
        let (workdir, fps, bitrate) = (args.workdir.clone(), args.fps, args.bitrate);
        std::thread::spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            let mut index = 0usize;
            loop {
                let path = workdir.join(format!("seg{index:05}.mp4"));
                let Ok(segment) =
                    open_segment(manager.get(), path, width, height, fps, bitrate, audio_tracks)
                else {
                    return;
                };
                if ready_tx.send(SendSegment(segment)).is_err() {
                    return; // la capture est terminée
                }
                index += 1;
            }
        })
    };

    let closer = std::thread::spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        for SendSegment(segment) in close_rx {
            match segment.close() {
                Ok(info) => {
                    if closed_tx.send(info).is_err() {
                        return;
                    }
                }
                Err(e) => tracing::error!("finalisation : {e:#}"),
            }
        }
    });

    let mut segment = ready_rx.recv().context("aucun segment initial")?.0;
    let mut segment_origin_hns = hns_since_boot(clock.origin().0, clock.frequency());
    let mut segment_first_frame = 0u64;

    // Coût de la rotation : c'est lui qui dirait si l'utilisateur perçoit un
    // à-coup toutes les deux secondes.
    let mut rotation_max_ms = 0.0f64;
    let mut rotation_total_ms = 0.0f64;
    let mut rotations = 0usize;

    let started = Instant::now();
    let mut next_tick = started;
    let mut has_content = false;

    for index in 0..total_frames {
        next_tick += tick;
        let slot = index as usize % TEXTURE_RING;

        match frame_pool.TryGetNextFrame() {
            Ok(frame) => {
                let surface = frame.Surface()?;
                let access: windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess =
                    surface.cast()?;
                let source: ID3D11Texture2D = unsafe { access.GetInterface()? };
                unsafe { context.CopyResource(&ring_textures[slot], &source) };
                has_content = true;
                drop(frame);
            }
            Err(_) if has_content => {
                let previous = (index as usize + TEXTURE_RING - 1) % TEXTURE_RING;
                unsafe { context.CopyResource(&ring_textures[slot], &ring_textures[previous]) };
            }
            Err(_) => {
                sleep_until(next_tick);
                continue;
            }
        }

        let pts = (index - segment_first_frame) as i64 * frame_duration;
        segment.write_video(&ring_textures[slot], pts, frame_duration)?;

        for chunk in rx.try_iter() {
            let pts = (chunk.boot_hns - segment_origin_hns).max(0);
            segment.write_audio(chunk.track, &chunk.pcm, pts)?;
        }

        // Rotation de segment.
        if (index + 1 - segment_first_frame) >= frames_per_segment {
            let t0 = Instant::now();
            let next = ready_rx.recv().context("plus de segment disponible")?.0;
            let previous = std::mem::replace(&mut segment, next);
            close_tx.send(SendSegment(previous)).ok();
            for info in closed_rx.try_iter() {
                ring.push(info);
            }
            segment_origin_hns = hns_since_boot(clock.now().0, clock.frequency());
            segment_first_frame = index + 1;

            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            rotation_max_ms = rotation_max_ms.max(ms);
            rotation_total_ms += ms;
            rotations += 1;
        }

        if index % (args.fps as u64 * 15) == 0 && index > 0 {
            tracing::info!(
                "t={:.0}s  segments={}  anneau={:.1}s / {:.0} Mo",
                started.elapsed().as_secs_f64(),
                ring.segments.len(),
                ring.total_hns() as f64 / HNS_PER_SEC as f64,
                ring.total_bytes() as f64 / 1_048_576.0
            );
        }

        sleep_until(next_tick);
    }

    // ── simulation du raccourci ──
    //
    // La finalisation du segment courant reste synchrone ici, et c'est
    // volontaire : c'est exactement le geste qu'il faut mesurer. Sans elle, le
    // segment en cours n'est pas lisible et l'on perd jusqu'à 2 s — l'instant
    // même que l'utilisateur voulait garder.
    let hotkey = Instant::now();
    let info = segment.close()?;
    let flush_ms = hotkey.elapsed().as_secs_f64() * 1000.0;

    drop(close_tx);
    let _ = closer.join();
    for pending in closed_rx.try_iter() {
        ring.push(pending);
    }
    ring.push(info);

    drop(ready_rx); // débloque l'ouvreur, qui sort de sa boucle
    let _ = opener.join();

    stop.store(true, Ordering::Relaxed);
    for h in handles {
        let _ = h.join();
    }

    let clip = args.workdir.join("clip.mp4");
    let concat_start = Instant::now();
    let (clip_bytes, samples) = concat_passthrough(&ring.segments, &clip)?;
    let concat_ms = concat_start.elapsed().as_secs_f64() * 1000.0;
    let save_ms = flush_ms + concat_ms;

    session.Close()?;
    frame_pool.Close()?;

    tracing::info!("─── Spike 4 terminé ───");
    tracing::info!("durée capturée      : {:.1}s", started.elapsed().as_secs_f64());
    tracing::info!(
        "rotation de segment : max {rotation_max_ms:.1} ms, moyenne {:.1} ms sur {rotations}",
        if rotations > 0 { rotation_total_ms / rotations as f64 } else { 0.0 }
    );
    tracing::info!(
        "anneau final        : {} segments, {:.1}s, {:.0} Mo",
        ring.segments.len(),
        ring.total_hns() as f64 / HNS_PER_SEC as f64,
        ring.total_bytes() as f64 / 1_048_576.0
    );
    tracing::info!(
        "purges              : {} par durée, {} par budget d'octets",
        ring.evicted_by_duration,
        ring.evicted_by_bytes
    );
    tracing::info!("");
    tracing::info!("SAUVEGARDE");
    tracing::info!("  finalisation du segment courant : {flush_ms:.0} ms");
    tracing::info!("  concaténation sans réencodage   : {concat_ms:.0} ms ({samples} échantillons)");
    tracing::info!("  total                           : {save_ms:.0} ms");
    tracing::info!(
        "  clip                            : {} ({:.1} Mo)",
        clip.display(),
        clip_bytes as f64 / 1_048_576.0
    );
    tracing::info!("");
    if save_ms < 1000.0 {
        tracing::info!("✅ CRITÈRE TENU : sauvegarde en {save_ms:.0} ms < 1000 ms");
    } else {
        tracing::error!("❌ CRITÈRE MANQUÉ : sauvegarde en {save_ms:.0} ms ≥ 1000 ms");
    }
    Ok(())
}

fn sleep_until(deadline: Instant) {
    let now = Instant::now();
    if deadline > now {
        std::thread::sleep(deadline - now);
    }
}

fn create_d3d_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let (mut device, mut context) = (None, None);
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    Ok((device.context("device nul")?, context.context("contexte nul")?))
}

fn create_texture_ring(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<Vec<ID3D11Texture2D>> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    (0..TEXTURE_RING)
        .map(|_| {
            let mut t = None;
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut t))? };
            t.context("texture nulle")
        })
        .collect()
}
