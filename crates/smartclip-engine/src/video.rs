//! Capture d'écran par Windows Graphics Capture.
//!
//! WGC plutôt qu'un hook Direct3D : aucune injection de DLL dans le jeu, donc
//! aucun risque de déclencher un anti-cheat. Les textures ne quittent jamais le
//! GPU — elles vont directement à l'encodeur matériel.

use anyhow::{Context, Result};
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{HMONITOR, MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint};
use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::Interface;

/// Nombre de textures intermédiaires. Elles découplent la cadence d'arrivée des
/// frames de celle de l'encodeur, qui peut avoir plusieurs frames en vol.
const TEXTURE_RING: usize = 8;

pub struct Capture {
    pub device: ID3D11Device,
    context: ID3D11DeviceContext,
    frame_pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    /// Conservé pour pouvoir interroger la taille courante du moniteur.
    item: GraphicsCaptureItem,
    textures: Vec<ID3D11Texture2D>,
    pub width: u32,
    pub height: u32,
    /// Tant qu'aucune frame n'est arrivée, il n'y a rien à répéter.
    has_content: bool,
}

impl Capture {
    /// Ouvre la capture du moniteur principal.
    pub fn primary_monitor() -> Result<Self> {
        let (device, context) = create_device()?;
        // L'encodeur et la capture touchent le même device depuis des threads
        // différents ; sans cette protection, D3D11 corrompt son état interne.
        let _ = unsafe { device.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true) };

        let monitor: HMONITOR =
            unsafe { MonitorFromPoint(Default::default(), MONITOR_DEFAULTTOPRIMARY) };
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(monitor)? };
        let size = item.Size()?;
        let (width, height) = (size.Width as u32, size.Height as u32);

        let dxgi: IDXGIDevice = device.cast()?;
        let winrt_device: IDirect3DDevice = unsafe {
            windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?
        }
        .cast()?;

        // Version « free threaded » : les frames arrivent sans dépendre d'une
        // boucle de messages, ce qui laisse le moteur libre de son cadencement.
        let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )?;
        let session = frame_pool.CreateCaptureSession(&item)?;
        // Supprime la bordure jaune de capture. Accepté sans privilège
        // particulier sur Windows 11 ; sur une version antérieure l'appel
        // échoue sans conséquence, d'où le `let _`.
        let _ = session.SetIsBorderRequired(false);

        let textures = create_textures(&device, width, height)?;

        Ok(Self {
            device,
            context,
            frame_pool,
            session,
            item,
            textures,
            width,
            height,
            has_content: false,
        })
    }

    /// Rend la nouvelle définition si le moniteur a changé de résolution.
    ///
    /// Un jeu qui bascule en plein écran la modifie très souvent — c'est-à-dire
    /// précisément au moment où l'utilisateur commence à jouer. Or la définition
    /// est figée dans le flux vidéo du segment : la capture ne peut pas
    /// continuer telle quelle, et la détecter est le seul moyen de ne pas
    /// enregistrer dans le vide.
    pub fn resolution_change(&self) -> Option<(u32, u32)> {
        let size = self.item.Size().ok()?;
        let (width, height) = (size.Width as u32, size.Height as u32);
        // Une taille nulle signale un moniteur momentanément absent (veille,
        // changement de sortie) : ce n'est pas un changement de définition.
        if width == 0 || height == 0 || (width == self.width && height == self.height) {
            return None;
        }
        Some((width, height))
    }

    pub fn start(&self) -> Result<()> {
        self.session.StartCapture()?;
        Ok(())
    }

    /// Indique si le périphérique Direct3D a été perdu.
    ///
    /// Une mise en veille, une mise à jour de pilote ou un plantage du GPU
    /// invalident le device : toutes les textures et l'encodeur deviennent
    /// inutilisables, et rien ne peut être réparé en place. C'est un cas
    /// fréquent — un poste qui dort chaque nuit le rencontre quotidiennement —
    /// et le seul recours est de tout reconstruire.
    pub fn device_lost(&self) -> Option<windows::core::Error> {
        // `GetDeviceRemovedReason` rend `Ok(())` tant que le device est sain,
        // et l'erreur qui l'a emporté sinon.
        unsafe { self.device.GetDeviceRemovedReason() }.err()
    }

    /// Prépare la texture correspondant à `index` et indique si l'image est
    /// nouvelle.
    ///
    /// Windows ne livre pas de frame quand rien ne bouge à l'écran. Pour tenir
    /// une cadence constante, on recopie alors la précédente. Renvoie `None`
    /// tant qu'aucune frame n'est jamais arrivée.
    pub fn next_texture(&mut self, index: u64) -> Result<Option<(&ID3D11Texture2D, bool)>> {
        let slot = index as usize % TEXTURE_RING;
        match self.frame_pool.TryGetNextFrame() {
            Ok(frame) => {
                let surface = frame.Surface()?;
                let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
                let source: ID3D11Texture2D = unsafe { access.GetInterface()? };
                unsafe { self.context.CopyResource(&self.textures[slot], &source) };
                self.has_content = true;
                drop(frame);
                Ok(Some((&self.textures[slot], true)))
            }
            Err(_) if self.has_content => {
                let previous = (index as usize + TEXTURE_RING - 1) % TEXTURE_RING;
                // Cloner une interface COM n'est qu'un AddRef : c'est la façon
                // la plus simple d'obtenir les deux emprunts distincts qu'exige
                // `CopyResource`.
                let source = self.textures[previous].clone();
                unsafe { self.context.CopyResource(&self.textures[slot], &source) };
                Ok(Some((&self.textures[slot], false)))
            }
            Err(_) => Ok(None),
        }
    }

    pub fn close(self) -> Result<()> {
        self.session.Close()?;
        self.frame_pool.Close()?;
        Ok(())
    }
}

fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let (mut device, mut context) = (None, None);
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            // VIDEO_SUPPORT est requis pour que l'encodeur matériel accepte nos
            // textures ; BGRA_SUPPORT l'est pour le format de WGC.
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    Ok((
        device.context("device D3D11 nul")?,
        context.context("contexte D3D11 nul")?,
    ))
}

fn create_textures(
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
