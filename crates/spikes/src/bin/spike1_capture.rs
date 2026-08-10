//! Spike 1 — Windows.Graphics.Capture → encodeur matériel → MP4.
//!
//! Risque validé : R (perfs / stabilité de la capture continue).
//! Question posée : peut-on encoder le moniteur principal pendant 10 minutes
//! sans aller-retour CPU, sans fuite mémoire et sans écrouler les FPS du jeu ?
//!
//! Chaîne : WGC (texture BGRA en VRAM) → CopyResource → IMFSample adossé à la
//! texture D3D11 → SinkWriter avec `MF_SINK_WRITER_D3D_MANAGER`. Media
//! Foundation insère alors le Video Processor MFT (BGRA→NV12) et l'encodeur
//! matériel NVENC / AMF / QuickSync, tous deux côté GPU. La mémoire système
//! n'est jamais touchée par les pixels.
//!
//! Code jetable : il répond à une question, il ne sera pas repris tel quel.
//!
//! Usage : `cargo run --release --bin spike1_capture -- --minutes 10`

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
    ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Gdi::{HMONITOR, MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMaxBitRate, CODECAPI_AVEncCommonMeanBitRate,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVGOPSize,
    ICodecAPI, IMFTransform, MFT_ENUM_HARDWARE_URL_Attribute, eAVEncCommonRateControlMode_CBR,
};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFAttributes, IMFDXGIDeviceManager, IMFMediaType, IMFSinkWriter, MF_MT_AVG_BITRATE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
    MF_SINK_WRITER_D3D_MANAGER, MF_SINK_WRITER_DISABLE_THROTTLING, MF_VERSION, MFCreateAttributes,
    MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateSample,
    MFCreateSinkWriterFromURL, MFMediaType_Video, MFSTARTUP_FULL, MFShutdown, MFStartup,
    MFVideoFormat_ARGB32, MFVideoFormat_H264, MFVideoInterlace_Progressive,
    eAVEncH264VProfile_High,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::GetCurrentProcess;
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::{GUID, HSTRING, Interface};

use aftermix_core::clock::{HNS_PER_SEC, MasterClock};

/// Nombre de textures dans l'anneau soumis à l'encodeur.
///
/// `WriteSample` est asynchrone : réécrire une texture encore référencée par
/// l'encodeur produirait du déchirement. Huit textures donnent ~133 ms de marge
/// à 60 fps, ce qui suffit largement pour un spike. Le moteur définitif devra
/// s'appuyer sur une `ID3D11Fence` plutôt que sur une marge empirique.
const TEXTURE_RING: usize = 8;

struct Args {
    minutes: f64,
    fps: u32,
    bitrate: u32,
    out: String,
    /// Valeur de `eAVEncCommonRateControlMode` : 0=CBR, 1=PeakConstrainedVBR,
    /// 2=UnconstrainedVBR, 3=Quality, 4=LowDelayVBR.
    rate_control: u32,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut args = Args {
            minutes: 1.0,
            fps: 60,
            bitrate: 20_000_000,
            out: "spike1.mp4".to_string(),
            rate_control: eAVEncCommonRateControlMode_CBR.0 as u32,
        };
        let mut it = std::env::args().skip(1);
        while let Some(flag) = it.next() {
            let mut value = || {
                it.next()
                    .with_context(|| format!("valeur manquante après {flag}"))
            };
            match flag.as_str() {
                "--minutes" => args.minutes = value()?.parse()?,
                "--fps" => args.fps = value()?.parse()?,
                "--bitrate" => args.bitrate = value()?.parse()?,
                "--out" => args.out = value()?,
                "--rc" => args.rate_control = value()?.parse()?,
                other => bail!("option inconnue : {other}"),
            }
        }
        Ok(args)
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse()?;

    unsafe {
        // MTA : la frame pool est créée en free-threaded et Media Foundation
        // appelle nos objets depuis ses propres threads.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL).context("MFStartup")?;
    }

    let result = run(&args);

    unsafe {
        let _ = MFShutdown();
        CoUninitialize();
    }

    result
}

fn run(args: &Args) -> Result<()> {
    let (device, context) = create_d3d_device().context("création du device D3D11")?;

    // Media Foundation pilote le device depuis ses threads d'encodage : sans
    // cette protection, la moindre commande concurrente corrompt le contexte.
    let multithread: ID3D11Multithread = device.cast()?;
    // Renvoie l'état précédent, dont on n'a rien à faire.
    let _ = unsafe { multithread.SetMultithreadProtected(true) };

    let item = capture_item_for_primary_monitor().context("GraphicsCaptureItem du moniteur")?;
    let size = item.Size()?;
    let (width, height) = (size.Width as u32, size.Height as u32);
    tracing::info!(width, height, fps = args.fps, "moniteur principal");

    let winrt_device = winrt_device_from(&device)?;
    let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &winrt_device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        size,
    )
    .context("création de la frame pool")?;

    let session = frame_pool.CreateCaptureSession(&item)?;
    // Windows 11 uniquement : supprime la bordure jaune autour de la zone
    // capturée. Sans la capability adéquate l'appel échoue — on n'en fait pas
    // un échec fatal, la bordure est cosmétique (risque R6).
    if let Err(e) = session.SetIsBorderRequired(false) {
        tracing::warn!("bordure de capture non désactivable : {e}");
    }

    let (writer, stream) = create_sink_writer(&device, args, width, height)
        .context("création du SinkWriter (encodeur matériel indisponible ?)")?;

    let ring = create_texture_ring(&device, width, height)?;

    session.StartCapture().context("StartCapture")?;
    unsafe { writer.BeginWriting() }.context("BeginWriting")?;

    // À faire impérativement après BeginWriting : c'est seulement à ce moment
    // que MF a résolu la topologie et instancié l'encodeur réel.
    describe_encoder(&writer, stream, args)?;

    let clock = MasterClock::new();
    let frame_duration_hns = HNS_PER_SEC / args.fps as i64;
    let tick = Duration::from_nanos(1_000_000_000 / args.fps as u64);
    let total_frames = (args.minutes * 60.0 * args.fps as f64).round() as u64;

    let started = Instant::now();
    let rss_start = working_set_bytes();
    let mut rss_peak = rss_start;
    let mut fresh_frames = 0u64; // frames réellement fournies par WGC
    let mut repeated = 0u64; // frames répétées (rien n'a bougé à l'écran)
    let mut has_content = false;
    let mut next_tick = started;

    for index in 0..total_frames {
        next_tick += tick;

        // WGC ne livre une frame que lorsque le contenu change. Pour produire
        // un flux à cadence constante — ce dont l'anneau de segments a besoin —
        // on répète la dernière texture connue quand rien n'a bougé.
        let slot = (index as usize) % TEXTURE_RING;
        match frame_pool.TryGetNextFrame() {
            Ok(frame) => {
                let surface = frame.Surface()?;
                let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
                let source: ID3D11Texture2D = unsafe { access.GetInterface()? };
                unsafe { context.CopyResource(&ring[slot], &source) };
                fresh_frames += 1;
                has_content = true;
                drop(frame);
            }
            Err(_) if has_content => {
                // Recopie la frame précédente dans le slot courant.
                let previous = (index as usize + TEXTURE_RING - 1) % TEXTURE_RING;
                unsafe { context.CopyResource(&ring[slot], &ring[previous]) };
                repeated += 1;
            }
            Err(_) => {
                // Aucune frame n'est encore arrivée : on n'a rien à encoder.
                sleep_until(next_tick);
                continue;
            }
        }

        write_texture(&writer, stream, &ring[slot], index as i64 * frame_duration_hns,
            frame_duration_hns)?;

        if index % (args.fps as u64 * 10) == 0 && index > 0 {
            let rss = working_set_bytes();
            rss_peak = rss_peak.max(rss);
            tracing::info!(
                t = format!("{:.0}s", started.elapsed().as_secs_f64()),
                frames = index,
                fraiches = fresh_frames,
                repetees = repeated,
                rss_mo = rss / 1_048_576,
                "progression"
            );
        }

        sleep_until(next_tick);
    }

    unsafe { writer.Finalize() }.context("Finalize")?;
    session.Close()?;
    frame_pool.Close()?;

    let elapsed = started.elapsed();
    let rss_end = working_set_bytes();
    let file_size = std::fs::metadata(&args.out).map(|m| m.len()).unwrap_or(0);

    tracing::info!("─── Spike 1 terminé ───");
    tracing::info!("durée réelle      : {:.1}s (cible {:.1}s)", elapsed.as_secs_f64(),
        args.minutes * 60.0);
    tracing::info!("frames encodées   : {total_frames} ({fresh_frames} fraîches, {repeated} répétées)");
    tracing::info!("cadence effective : {:.1} fps", total_frames as f64 / elapsed.as_secs_f64());
    tracing::info!("fichier           : {} ({:.1} Mo)", args.out, file_size as f64 / 1_048_576.0);
    tracing::info!("débit réel        : {:.1} Mbps",
        file_size as f64 * 8.0 / elapsed.as_secs_f64() / 1_000_000.0);
    tracing::info!("RSS début → fin   : {} Mo → {} Mo (pic {} Mo)",
        rss_start / 1_048_576, rss_end / 1_048_576, rss_peak / 1_048_576);

    // Écart entre la timeline CFR écrite dans le MP4 et le temps réel mesuré au
    // QPC. C'est exactement la dérive que le Spike 3 devra annuler quand des
    // pistes audio à horloge indépendante viendront s'ajouter : si elle est
    // déjà importante ici, le passage à un horodatage QPC par frame devient
    // obligatoire dès la V1 plutôt qu'une optimisation ultérieure.
    let cfr_hns = total_frames as i64 * frame_duration_hns;
    let wall_hns = clock.hns_since_origin(clock.now());
    tracing::info!("dérive CFR vs QPC : {:+.1} ms sur {:.0}s",
        (cfr_hns - wall_hns) as f64 / 10_000.0, elapsed.as_secs_f64());

    // Critère de sortie du spike : la mémoire ne doit pas croître avec la durée.
    let growth = rss_end.saturating_sub(rss_start);
    if growth > 64 * 1_048_576 {
        tracing::error!("ÉCHEC : +{} Mo de RSS, fuite probable", growth / 1_048_576);
    } else {
        tracing::info!("OK : croissance mémoire contenue (+{} Mo)", growth / 1_048_576);
    }

    Ok(())
}

/// Attente jusqu'à l'échéance, sans dériver : on vise un instant absolu et non
/// un `sleep(tick)` cumulatif qui prendrait du retard à chaque itération.
fn sleep_until(deadline: Instant) {
    let now = Instant::now();
    if deadline > now {
        std::thread::sleep(deadline - now);
    }
}

fn create_d3d_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            // BGRA_SUPPORT est exigé par WGC, VIDEO_SUPPORT par le Video
            // Processor MFT qui fera la conversion BGRA→NV12.
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    let device = device.context("device D3D11 nul")?;

    // La machine peut exposer des adaptateurs virtuels (Parsec, Meta, etc.).
    // `D3D_DRIVER_TYPE_HARDWARE` prend l'adaptateur par défaut : il faut savoir
    // lequel, sinon on encoderait sur un GPU virtuel sans bloc NVENC/AMF.
    if let Ok(dxgi) = device.cast::<IDXGIDevice>() {
        if let Ok(adapter) = unsafe { dxgi.GetAdapter() } {
            if let Ok(desc) = unsafe { adapter.GetDesc() } {
                let name = String::from_utf16_lossy(&desc.Description);
                tracing::info!(
                    "adaptateur : {} ({} Mo VRAM)",
                    name.trim_end_matches('\0'),
                    desc.DedicatedVideoMemory / 1_048_576
                );
            }
        }
    }

    Ok((device, context.context("contexte D3D11 nul")?))
}

fn winrt_device_from(device: &ID3D11Device) -> Result<IDirect3DDevice> {
    let dxgi: IDXGIDevice = device.cast()?;
    let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? };
    Ok(inspectable.cast()?)
}

fn capture_item_for_primary_monitor() -> Result<GraphicsCaptureItem> {
    let monitor: HMONITOR =
        unsafe { MonitorFromPoint(Default::default(), MONITOR_DEFAULTTOPRIMARY) };
    let interop: IGraphicsCaptureItemInterop =
        windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    Ok(unsafe { interop.CreateForMonitor(monitor)? })
}

/// Anneau de textures intermédiaires entre la frame pool et l'encodeur.
///
/// Les textures de la pool sont recyclées par WGC dès qu'on relâche la frame :
/// on ne peut donc pas les passer directement à un encodeur asynchrone.
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
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };

    (0..TEXTURE_RING)
        .map(|_| {
            let mut texture = None;
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture))? };
            texture.context("CreateTexture2D a renvoyé null")
        })
        .collect()
}

fn create_sink_writer(
    device: &ID3D11Device,
    args: &Args,
    width: u32,
    height: u32,
) -> Result<(IMFSinkWriter, u32)> {
    // Le device manager est ce qui permet à l'encodeur de consommer des
    // textures D3D11 directement, sans jamais rapatrier les pixels en RAM.
    let mut reset_token = 0u32;
    let mut manager: Option<IMFDXGIDeviceManager> = None;
    unsafe { MFCreateDXGIDeviceManager(&mut reset_token, &mut manager)? };
    let manager = manager.context("IMFDXGIDeviceManager nul")?;
    unsafe { manager.ResetDevice(device, reset_token)? };

    let mut attributes: Option<IMFAttributes> = None;
    unsafe { MFCreateAttributes(&mut attributes, 3)? };
    let attributes = attributes.context("IMFAttributes nul")?;
    unsafe {
        attributes.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
        // Sans cela le writer bride l'écriture sur le temps réel, ce qui fausse
        // la mesure et ferait accumuler du retard sur les pics de charge.
        attributes.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
        attributes.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, &manager)?;
    }

    let writer = unsafe {
        MFCreateSinkWriterFromURL(&HSTRING::from(args.out.as_str()), None, &attributes)?
    };

    let output = unsafe { MFCreateMediaType()? };
    unsafe {
        output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        output.SetUINT32(&MF_MT_AVG_BITRATE, args.bitrate)?;
        output.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        output.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
    }
    set_size(&output, &MF_MT_FRAME_SIZE, width, height)?;
    set_ratio(&output, &MF_MT_FRAME_RATE, args.fps, 1)?;
    set_ratio(&output, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;

    let stream = unsafe { writer.AddStream(&output)? };

    // Entrée en BGRA : MF insère lui-même le convertisseur vers NV12, exécuté
    // sur le GPU puisque le device manager est fourni.
    let input = unsafe { MFCreateMediaType()? };
    unsafe {
        input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_ARGB32)?;
        input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    }
    set_size(&input, &MF_MT_FRAME_SIZE, width, height)?;
    set_ratio(&input, &MF_MT_FRAME_RATE, args.fps, 1)?;
    set_ratio(&input, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;

    unsafe { writer.SetInputMediaType(stream, &input, None)? };

    Ok((writer, stream))
}

/// Identifie l'encodeur réellement instancié et lui impose son débit.
///
/// Deux constats du premier essai motivent cette fonction :
/// - `MF_MT_AVG_BITRATE` sur le type de sortie est traité comme une simple
///   indication ; le débit observé faisait le double de la consigne. Le seul
///   réglage qui engage l'encodeur passe par `ICodecAPI`.
/// - la présence de `MFT_ENUM_HARDWARE_URL_Attribute` sur l'instance est le
///   marqueur fiable d'un MFT matériel. Sans lui, MF est retombé sur l'encodeur
///   logiciel Microsoft et toutes les mesures de perf sont à jeter.
fn describe_encoder(writer: &IMFSinkWriter, stream: u32, args: &Args) -> Result<()> {
    let transform: IMFTransform = unsafe {
        let mut raw = std::ptr::null_mut();
        writer.GetServiceForStream(stream, &GUID::zeroed(), &IMFTransform::IID, &mut raw)?;
        IMFTransform::from_raw(raw)
    };

    let attributes = unsafe { transform.GetAttributes() };
    let hardware = match &attributes {
        Ok(attrs) => unsafe { attrs.GetStringLength(&MFT_ENUM_HARDWARE_URL_Attribute).is_ok() },
        Err(_) => false,
    };
    if hardware {
        tracing::info!("encodeur : MFT matériel (NVENC / AMF / QuickSync)");
    } else {
        tracing::warn!(
            "encodeur : MFT LOGICIEL — les mesures de perf de ce spike ne valent rien"
        );
    }

    // Débit : rate control explicite, sinon l'encodeur choisit tout seul.
    if let Ok(codec) = transform.cast::<ICodecAPI>() {
        unsafe {
            let mode = VARIANT::from(args.rate_control);
            if let Err(e) = codec.SetValue(&CODECAPI_AVEncCommonRateControlMode, &mode) {
                tracing::warn!("mode de rate control refusé : {e}");
            }
            let mean = VARIANT::from(args.bitrate);
            if let Err(e) = codec.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &mean) {
                tracing::warn!("débit moyen refusé : {e}");
            }
            // Le MFT AMD dépasse sa consigne de débit moyen d'un facteur 2 ;
            // seul un plafond explicite le contraint réellement.
            let max = VARIANT::from(args.bitrate);
            if let Err(e) = codec.SetValue(&CODECAPI_AVEncCommonMaxBitRate, &max) {
                tracing::warn!("débit crête refusé : {e}");
            }
            // GOP de 2 s : c'est la granularité que l'anneau de segments du
            // Spike 4 exigera, autant la fixer dès maintenant.
            let gop = VARIANT::from(args.fps * 2);
            if let Err(e) = codec.SetValue(&CODECAPI_AVEncMPVGOPSize, &gop) {
                tracing::warn!("taille de GOP refusée : {e}");
            }
        }
    } else {
        tracing::warn!("ICodecAPI indisponible : le débit restera au choix de l'encodeur");
    }

    // Relecture : le débit observé faisait exactement le double de la consigne
    // sur les deux premiers essais. On interroge donc l'encodeur sur ce qu'il
    // pense avoir retenu, plutôt que de supposer.
    if let Ok(codec) = transform.cast::<ICodecAPI>() {
        for (nom, guid) in [
            ("mean bitrate", CODECAPI_AVEncCommonMeanBitRate),
            ("rate control", CODECAPI_AVEncCommonRateControlMode),
            ("GOP size", CODECAPI_AVEncMPVGOPSize),
        ] {
            match unsafe { codec.GetValue(&guid) } {
                Ok(v) => tracing::info!("  relecture {nom:<13} = {}", variant_debug(&v)),
                Err(e) => tracing::info!("  relecture {nom:<13} : indisponible ({})", e.code().0),
            }
        }
    }

    // Ce que le MFT expose réellement comme type de sortie, une fois négocié.
    if let Ok(output) = unsafe { transform.GetOutputCurrentType(0) } {
        let bitrate = unsafe { output.GetUINT32(&MF_MT_AVG_BITRATE) }.unwrap_or(0);
        let rate = unsafe { output.GetUINT64(&MF_MT_FRAME_RATE) }.unwrap_or(0);
        tracing::info!(
            "  type de sortie négocié : {} Mbps, {}/{} fps",
            bitrate / 1_000_000,
            rate >> 32,
            rate & 0xFFFF_FFFF
        );
    }

    Ok(())
}

fn variant_debug(v: &VARIANT) -> String {
    u32::try_from(v)
        .map(|n| n.to_string())
        .unwrap_or_else(|_| "<type non entier>".to_string())
}

fn write_texture(
    writer: &IMFSinkWriter,
    stream: u32,
    texture: &ID3D11Texture2D,
    time_hns: i64,
    duration_hns: i64,
) -> Result<()> {
    unsafe {
        let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)?;
        // Un buffer DXGI naît avec une longueur courante nulle ; sans cela le
        // MFT en aval considère l'échantillon comme vide.
        let length = buffer.cast::<IMF2DBuffer>()?.GetContiguousLength()?;
        buffer.SetCurrentLength(length)?;

        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(time_hns)?;
        sample.SetSampleDuration(duration_hns)?;
        writer.WriteSample(stream, &sample)?;
    }
    Ok(())
}

/// Les deux GUID de taille et de ratio de MF encodent deux `u32` dans un `u64`,
/// poids fort en premier.
fn set_size(media_type: &IMFMediaType, key: &GUID, high: u32, low: u32) -> Result<()> {
    unsafe { media_type.SetUINT64(key, ((high as u64) << 32) | low as u64)? };
    Ok(())
}

fn set_ratio(media_type: &IMFMediaType, key: &GUID, numerator: u32, denominator: u32) -> Result<()> {
    set_size(media_type, key, numerator, denominator)
}

fn working_set_bytes() -> u64 {
    let mut counters = PROCESS_MEMORY_COUNTERS::default();
    let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    unsafe {
        if GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, size).is_ok() {
            counters.WorkingSetSize as u64
        } else {
            0
        }
    }
}
