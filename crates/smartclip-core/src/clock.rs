//! Horloge de référence unique du moteur.
//!
//! Toutes les sources horodatent leurs échantillons sur ce même compteur QPC :
//! - la vidéo via `Direct3D11CaptureFrame::SystemRelativeTime`,
//! - chaque piste audio via le `pu64QPCPosition` de `IAudioCaptureClient::GetBuffer`.
//!
//! C'est la condition nécessaire pour muxer N pistes sans dérive (risque R1).
//! Aucun module de capture ne doit appeler `Instant::now` : les instants issus
//! des API Windows sont déjà en ticks QPC, les convertir en temps monotone Rust
//! réintroduirait exactement l'erreur qu'on cherche à éliminer.

use std::time::Duration;

use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

/// Unité de temps de Media Foundation : 100 nanosecondes.
pub const HNS_PER_SEC: i64 = 10_000_000;

/// Un instant exprimé en ticks QPC bruts, tel que fourni par Windows.
///
/// Volontairement pas convertible en `Instant` : la conversion doit toujours
/// passer par la [`MasterClock`] qui connaît la fréquence et l'origine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct QpcInstant(pub i64);

impl QpcInstant {
    /// Depuis la valeur `u64` que renvoient les API audio WASAPI.
    pub fn from_u64(ticks: u64) -> Self {
        Self(ticks as i64)
    }
}

/// Horloge maître du moteur de capture.
///
/// Instanciée une seule fois au démarrage d'une session ; son origine définit
/// le zéro de la timeline de tous les segments produits.
#[derive(Debug, Clone)]
pub struct MasterClock {
    freq: i64,
    origin: i64,
}

impl MasterClock {
    /// Démarre une horloge dont l'origine est l'instant présent.
    ///
    /// `QueryPerformanceFrequency` ne peut pas échouer sur Windows XP et
    /// ultérieur, et `QueryPerformanceCounter` non plus — d'où l'absence de
    /// `Result` dans cette signature.
    pub fn new() -> Self {
        let mut freq = 0i64;
        let mut origin = 0i64;
        unsafe {
            QueryPerformanceFrequency(&mut freq).expect("QueryPerformanceFrequency");
            QueryPerformanceCounter(&mut origin).expect("QueryPerformanceCounter");
        }
        debug_assert!(freq > 0);
        Self { freq, origin }
    }

    /// Fréquence du compteur, en ticks par seconde.
    pub fn frequency(&self) -> i64 {
        self.freq
    }

    /// Origine de la timeline, en ticks QPC absolus.
    pub fn origin(&self) -> QpcInstant {
        QpcInstant(self.origin)
    }

    /// Lecture du compteur à l'instant présent.
    pub fn now(&self) -> QpcInstant {
        let mut ticks = 0i64;
        unsafe {
            QueryPerformanceCounter(&mut ticks).expect("QueryPerformanceCounter");
        }
        QpcInstant(ticks)
    }

    /// Position d'un instant sur la timeline, en unités de 100 ns.
    ///
    /// Peut être négative : un paquet audio peut légitimement porter un
    /// horodatage antérieur à l'origine de l'horloge, parce que le périphérique
    /// l'avait déjà mis en tampon avant qu'on démarre. C'est au muxeur de
    /// décider s'il le rogne ou le rejette, pas à l'horloge.
    pub fn hns_since_origin(&self, at: QpcInstant) -> i64 {
        ticks_to_hns(at.0 - self.origin, self.freq)
    }

    /// Temps écoulé depuis l'origine, saturé à zéro.
    pub fn elapsed(&self) -> Duration {
        let hns = self.hns_since_origin(self.now()).max(0);
        Duration::from_nanos(hns as u64 * 100)
    }
}

impl Default for MasterClock {
    fn default() -> Self {
        Self::new()
    }
}

/// Convertit une durée en ticks QPC vers des unités de 100 ns.
///
/// Passe par `i128` : à 10 MHz, `ticks * HNS_PER_SEC` déborde un `i64` au bout
/// d'une trentaine de minutes de capture, ce qui est très exactement le régime
/// visé par un buffer permanent.
fn ticks_to_hns(ticks: i64, freq: i64) -> i64 {
    ((ticks as i128 * HNS_PER_SEC as i128) / freq as i128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversion_sans_debordement_sur_longue_duree() {
        // 8 heures de buffer permanent sur un compteur à 10 MHz : le cas qui
        // déborderait si le calcul restait en i64.
        let freq = 10_000_000;
        let ticks = 8 * 3600 * freq;
        assert_eq!(ticks_to_hns(ticks, freq), 8 * 3600 * HNS_PER_SEC);
    }

    #[test]
    fn instants_anterieurs_a_lorigine_restent_negatifs() {
        let clock = MasterClock::new();
        let avant = QpcInstant(clock.origin().0 - clock.frequency()); // 1 s avant
        assert_eq!(clock.hns_since_origin(avant), -HNS_PER_SEC);
    }

    #[test]
    fn lhorloge_avance() {
        let clock = MasterClock::new();
        let t0 = clock.hns_since_origin(clock.now());
        std::thread::sleep(Duration::from_millis(20));
        let t1 = clock.hns_since_origin(clock.now());
        assert!(t1 > t0, "t1={t1} devrait dépasser t0={t0}");
        // 20 ms = 200_000 hns ; on tolère largement l'imprécision du scheduler.
        assert!(t1 - t0 >= 150_000, "écart trop faible : {}", t1 - t0);
    }
}
