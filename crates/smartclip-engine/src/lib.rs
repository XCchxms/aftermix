//! Moteur SmartClip : buffer vidéo permanent et sauvegarde multi-pistes.
//!
//! Ce crate est la mise en commun des quatre prototypes de la Phase 0, dont
//! chacun a validé un risque avant d'être écrit ici. Les contraintes qu'ils ont
//! révélées sont reportées en commentaire à l'endroit qu'elles gouvernent —
//! elles ne sont pas négociables, chacune a coûté une session de diagnostic.
//!
//! Point d'entrée : [`Recorder::start`], puis [`Recorder::save`] pour figer les
//! dernières secondes dans un fichier.

pub mod audio;
pub mod concat;
pub mod export;
pub mod library;
pub mod recorder;
pub mod segment;
pub mod video;
pub mod voice;

pub use export::{ClipInfo, MixOutcome, mix_and_export};
pub use library::{Clip, ClipMeta};
pub use recorder::{Recorder, SaveOutcome};

use smartclip_core::clock::HNS_PER_SEC;

/// Fréquence d'échantillonnage de travail, commune à toutes les pistes.
pub const SAMPLE_RATE: u32 = 48_000;
/// Toutes les pistes sont stéréo : c'est ce que rend le loopback WASAPI et ce
/// qu'attend l'encodeur AAC.
pub const CHANNELS: u16 = 2;

/// Convertit des ticks QPC en « 100 ns depuis le démarrage de la machine ».
///
/// C'est le pivot de toute la synchronisation : `SystemRelativeTime` de Windows
/// Graphics Capture est déjà dans cette unité, et cette fonction y amène les
/// horodatages audio. Vidéo et audio deviennent alors directement comparables,
/// ce qui rend la dérive nulle par construction plutôt que mesurée.
pub fn hns_since_boot(ticks: i64, frequency: i64) -> i64 {
    ((ticks as i128 * HNS_PER_SEC as i128) / frequency as i128) as i64
}

/// Réglages du buffer permanent.
#[derive(Debug, Clone)]
pub struct Config {
    /// Durée conservée en arrière, en secondes (30, 60, 180 ou 300 dans l'UI).
    pub buffer_seconds: f64,
    /// Plafond disque en octets.
    ///
    /// Ce n'est pas une ceinture de sécurité optionnelle. Le MFT matériel AMD
    /// ignore `MF_MT_AVG_BITRATE` comme `AVEncCommonMaxBitRate` et produit
    /// jusqu'au double de la consigne ; sans plafond en octets, la durée seule
    /// ne borne rien. Compter ~5,3 Mo/s réels à 1080p60 avec 4 pistes, soit
    /// ~1,6 Go pour 5 minutes.
    pub max_bytes: u64,
    /// Durée d'un segment.
    ///
    /// Contre-intuitivement, la raccourcir ne réduit pas ce qu'on perd à la
    /// sauvegarde : le segment courant est finalisé à la demande, donc rien
    /// n'est perdu quelle que soit sa durée. Elle ne joue que sur deux choses :
    /// la granularité de la purge, et surtout **le coût de la sauvegarde**.
    ///
    /// Le recollage ouvre un `IMFSourceReader` par segment, et chaque ouverture
    /// initialise un pipeline Media Foundation — de loin le poste dominant.
    /// Mesuré : un buffer de 58 s en segments de 2 s (30 fichiers) se
    /// sauvegardait en 5,6 s, très au-dessus de la seconde visée.
    pub segment_seconds: f64,
    pub fps: u32,
    pub bitrate: u32,
    /// Nombre maximum d'applications capturées séparément, micro non compris.
    pub max_sources: usize,
    /// Nombre d'emplacements de pistes réservés dans chaque segment.
    ///
    /// Il est fixe et non négociable une fois le buffer démarré : le recollage
    /// exige que tous les segments partagent la même structure de flux. Les
    /// applications lancées en cours de session prennent un emplacement libre,
    /// et ceux qui restent vacants sont remplis de silence — une piste déclarée
    /// mais jamais alimentée empêcherait la finalisation du segment.
    pub track_slots: usize,
    /// Enregistre le micro sur sa piste dédiée.
    ///
    /// Le couper garde l'emplacement réservé et rempli de silence : la
    /// structure des flux est identique d'un segment à l'autre, condition du
    /// recollage, et l'utilisateur peut rebrancher son micro sans vider le
    /// buffer.
    pub capture_microphone: bool,
    /// Périphérique d'entrée à utiliser. `None` = celui de Windows.
    pub microphone: Option<String>,
    /// Dossier des segments en cours. Purgé au démarrage.
    pub workdir: std::path::PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            buffer_seconds: 60.0,
            max_bytes: 2 * 1024 * 1024 * 1024,
            segment_seconds: 8.0,
            fps: 60,
            bitrate: 20_000_000,
            max_sources: 4,
            // 4 applications + le micro, plus un emplacement d'avance pour une
            // application lancée après le démarrage.
            track_slots: 6,
            capture_microphone: true,
            microphone: None,
            workdir: std::env::temp_dir().join("smartclip"),
        }
    }
}
