//! Segments MP4 et anneau borné.
//!
//! Le buffer permanent est une suite de petits MP4 autonomes plutôt qu'un seul
//! gros fichier : purger revient à effacer les plus anciens, et sauvegarder à
//! recoller les récents sans rien réencoder.

use std::path::PathBuf;

use anyhow::{Context, Result};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFAttributes, IMFDXGIDeviceManager, IMFMediaType, IMFSinkWriter,
    MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT,
    MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
    MF_SINK_WRITER_D3D_MANAGER, MF_SINK_WRITER_DISABLE_THROTTLING, MFAudioFormat_AAC,
    MFAudioFormat_PCM, MFCreateAttributes, MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFCreateSinkWriterFromURL,
    MFMediaType_Audio, MFMediaType_Video, MFVideoFormat_ARGB32, MFVideoFormat_H264,
    MFVideoInterlace_Progressive, eAVEncH264VProfile_High,
};
use windows::core::{GUID, HSTRING, Interface};

use smartclip_core::clock::HNS_PER_SEC;

use crate::{CHANNELS, SAMPLE_RATE};

/// Fabrique de segments : conserve ce qui doit l'être entre deux ouvertures.
pub struct SegmentFactory {
    manager: IMFDXGIDeviceManager,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    pub audio_tracks: usize,
}

// Le manager est transféré vers le thread d'ouverture, jamais partagé pendant
// un appel. Tous les threads du moteur sont en MTA, où COM garantit l'accès.
unsafe impl Send for SegmentFactory {}

impl SegmentFactory {
    pub fn new(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        audio_tracks: usize,
    ) -> Result<Self> {
        let mut token = 0u32;
        let mut manager: Option<IMFDXGIDeviceManager> = None;
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager)? };
        let manager = manager.context("IMFDXGIDeviceManager nul")?;
        unsafe { manager.ResetDevice(device, token)? };
        Ok(Self {
            manager,
            width,
            height,
            fps,
            bitrate,
            audio_tracks,
        })
    }

    /// Ouvre un segment prêt à recevoir des échantillons.
    ///
    /// À faire **hors de la boucle de capture** : la création d'un SinkWriter
    /// réinitialise le MFT matériel et coûte plusieurs centaines de
    /// millisecondes.
    pub fn open(&self, path: PathBuf) -> Result<Segment> {
        let mut attributes: Option<IMFAttributes> = None;
        unsafe { MFCreateAttributes(&mut attributes, 3)? };
        let attributes = attributes.context("attributs nuls")?;
        unsafe {
            attributes.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
            // La régulation du SinkWriter reste ACTIVE.
            //
            // La désactiver (`MF_SINK_WRITER_DISABLE_THROTTLING`) supprime toute
            // contre-pression : dès que l'encodeur prend du retard, le writer
            // continue d'accepter des échantillons et les empile. Chacun retient
            // une texture 1080p de 8 Mo. Une campagne de 30 min a vu la mémoire
            // passer de 87 Mo à 4,6 Go après un incident d'encodage.
            //
            // Avec la régulation, `WriteSample` peut bloquer brièvement et l'on
            // perd quelques images — un compromis sans commune mesure.
            attributes.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 0)?;
            // Sans ce manager, le SinkWriter rapatrie chaque frame en mémoire
            // centrale : c'est la différence entre 5 % et 60 % de CPU.
            attributes.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, &self.manager)?;
        }

        let writer = unsafe {
            MFCreateSinkWriterFromURL(
                &HSTRING::from(path.to_string_lossy().as_ref()),
                None,
                &attributes,
            )?
        };

        let out = unsafe { MFCreateMediaType()? };
        unsafe {
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, self.bitrate)?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
        }
        pack(&out, &MF_MT_FRAME_SIZE, self.width, self.height)?;
        pack(&out, &MF_MT_FRAME_RATE, self.fps, 1)?;
        pack(&out, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
        let video_stream = unsafe { writer.AddStream(&out)? };

        let inp = unsafe { MFCreateMediaType()? };
        unsafe {
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_ARGB32)?;
            inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        }
        pack(&inp, &MF_MT_FRAME_SIZE, self.width, self.height)?;
        pack(&inp, &MF_MT_FRAME_RATE, self.fps, 1)?;
        pack(&inp, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
        unsafe { writer.SetInputMediaType(video_stream, &inp, None)? };

        // Tous les flux doivent être déclarés avant BeginWriting.
        let mut audio_streams = Vec::with_capacity(self.audio_tracks);
        for _ in 0..self.audio_tracks {
            let out = unsafe { MFCreateMediaType()? };
            unsafe {
                out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
                out.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
                out.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                out.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)?;
                out.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS as u32)?;
                out.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 24_000)?; // 192 kbps
            }
            let stream = unsafe { writer.AddStream(&out)? };

            let inp = unsafe { MFCreateMediaType()? };
            unsafe {
                inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
                inp.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
                inp.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                inp.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)?;
                inp.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS as u32)?;
                inp.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, (CHANNELS * 2) as u32)?;
                writer.SetInputMediaType(stream, &inp, None)?;
            }
            audio_streams.push(stream);
        }

        unsafe { writer.BeginWriting()? };
        Ok(Segment {
            writer,
            video_stream,
            audio_streams,
            path,
            end_hns: 0,
            has_samples: false,
        })
    }
}

fn pack(media_type: &IMFMediaType, key: &GUID, high: u32, low: u32) -> Result<()> {
    unsafe { media_type.SetUINT64(key, ((high as u64) << 32) | low as u64)? };
    Ok(())
}

/// Un MP4 autonome dont les horodatages repartent de zéro.
///
/// Un SinkWriter neuf émet toujours une IDR en première frame : la frontière de
/// segment est donc une frontière de GOP par construction, ce qui rend le
/// recollage possible sans réencodage.
pub struct Segment {
    writer: IMFSinkWriter,
    video_stream: u32,
    audio_streams: Vec<u32>,
    path: PathBuf,
    /// Fin de la timeline **vidéo**, qui donne sa durée au segment.
    end_hns: i64,
    /// Vrai dès qu'un échantillon, vidéo ou audio, a été écrit.
    ///
    /// Distinct de `end_hns` : un segment peut avoir reçu de l'audio sans
    /// aucune image — écran parfaitement immobile, WGC ne livrant alors rien.
    /// Il est finalisable, même si sa durée vidéo est nulle. Confondre les deux
    /// notions faisait écarter tous les segments et produisait un clip vide.
    has_samples: bool,
}

// Transféré entre le thread d'ouverture, la boucle de capture et le thread de
// finalisation — jamais touché par deux à la fois.
unsafe impl Send for Segment {}

impl Segment {
    pub fn write_video(
        &mut self,
        texture: &ID3D11Texture2D,
        pts_hns: i64,
        duration_hns: i64,
    ) -> Result<()> {
        unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)?;
            let length = buffer.cast::<IMF2DBuffer>()?.GetContiguousLength()?;
            buffer.SetCurrentLength(length)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(pts_hns)?;
            sample.SetSampleDuration(duration_hns)?;
            self.writer.WriteSample(self.video_stream, &sample)?;
        }
        // **Seule la vidéo définit la durée du segment.**
        //
        // La calculer sur tous les flux la laissait polluer par les
        // horodatages audio : un paquet arrivé en retard porte une position
        // lointaine, qui gonflait la durée déclarée. Au recollage, l'offset du
        // segment suivant sautait d'autant et creusait un trou dans la vidéo —
        // jusqu'à 24 secondes sans la moindre image, pendant que le son
        // continuait. C'est la timeline vidéo qui fait foi.
        self.end_hns = self.end_hns.max(pts_hns + duration_hns);
        self.has_samples = true;
        Ok(())
    }

    pub fn write_audio(&mut self, track: usize, pcm: &[i16], pts_hns: i64) -> Result<()> {
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
            sample.SetSampleTime(pts_hns)?;
            sample.SetSampleDuration(duration)?;
            self.writer.WriteSample(stream, &sample)?;
        }
        // L'audio n'avance pas `end_hns` — voir `write_video` — mais il rend
        // bien le segment finalisable.
        self.has_samples = true;
        Ok(())
    }

    /// Ferme le segment et le rend lisible.
    ///
    /// C'est l'opération à déclencher immédiatement à l'appui du raccourci :
    /// sans elle le segment en cours est inexploitable, et l'on perd jusqu'à sa
    /// durée entière — c'est-à-dire précisément l'instant à sauver.
    pub fn close(self) -> Result<SegmentInfo> {
        // Un segment qui n'a rien reçu ne se finalise pas.
        //
        // `Finalize` échoue alors avec MF_E_SINK_NO_SAMPLES_PROCESSED
        // (0xC00D4A44) et fait échouer toute la sauvegarde. Le cas est banal :
        // il suffit que l'utilisateur appuie sur le raccourci dans la fraction
        // de seconde qui suit une rotation de segment. Le fichier vide est
        // simplement écarté.
        if !self.has_samples {
            drop(self.writer);
            let _ = std::fs::remove_file(&self.path);
            return Ok(SegmentInfo {
                path: self.path,
                bytes: 0,
                duration_hns: 0,
            });
        }
        unsafe { self.writer.Finalize()? };
        let bytes = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        Ok(SegmentInfo {
            path: self.path,
            bytes,
            duration_hns: self.end_hns,
        })
    }
}

/// Numéro de séquence d'un fichier `segNNNNNN.mp4`.
fn sequence(path: &std::path::Path) -> Option<u64> {
    path.file_stem()?
        .to_str()?
        .strip_prefix("seg")?
        .parse()
        .ok()
}

#[derive(Debug, Clone)]
pub struct SegmentInfo {
    pub path: PathBuf,
    pub bytes: u64,
    pub duration_hns: i64,
}

/// Anneau borné **à la fois** en durée et en octets.
///
/// La double borne n'est pas une précaution de principe. Le MFT matériel AMD
/// ignore tout plafond de débit et produit jusqu'au double de la consigne : une
/// borne en durée seule laisserait le disque se remplir sans limite connue.
pub struct SegmentRing {
    segments: Vec<SegmentInfo>,
    max_duration_hns: i64,
    max_bytes: u64,
}

impl SegmentRing {
    pub fn new(max_seconds: f64, max_bytes: u64) -> Self {
        Self {
            segments: Vec::new(),
            max_duration_hns: (max_seconds * HNS_PER_SEC as f64) as i64,
            max_bytes,
        }
    }

    pub fn duration_hns(&self) -> i64 {
        self.segments.iter().map(|s| s.duration_hns).sum()
    }

    pub fn bytes(&self) -> u64 {
        self.segments.iter().map(|s| s.bytes).sum()
    }

    pub fn segments(&self) -> &[SegmentInfo] {
        &self.segments
    }

    /// Ajoute un segment et purge les plus anciens tant qu'une borne est
    /// dépassée. On garde toujours au moins un segment, sans quoi une borne
    /// trop serrée viderait le buffer entièrement.
    pub fn push(&mut self, segment: SegmentInfo) {
        // Les segments sans contenu — rotation suivie d'une sauvegarde
        // immédiate — n'ont rien à apporter au buffer.
        if segment.bytes == 0 {
            return;
        }
        self.segments.push(segment);
        while self.segments.len() > 1
            && (self.duration_hns() > self.max_duration_hns || self.bytes() > self.max_bytes)
        {
            let old = self.segments.remove(0);
            let _ = std::fs::remove_file(&old.path);
        }
    }

    /// Supprime les fichiers du dossier de travail qui n'appartiennent plus à
    /// l'anneau.
    ///
    /// Filet de sécurité indispensable : un segment n'entre dans l'anneau que
    /// s'il a été correctement fermé *et* que son information est revenue au
    /// moteur. Tout ce qui échappe à ce chemin — finalisation en échec, arrêt
    /// brutal, session précédente — resterait sinon sur le disque pour
    /// toujours. Une campagne de 30 min a laissé 935 Mo d'orphelins, que le
    /// cache d'écriture de Windows impute au processus : la « fuite mémoire »
    /// observée en découlait directement.
    ///
    /// Seuls les fichiers antérieurs au plus ancien segment retenu sont
    /// effacés : le segment en cours d'écriture et celui pré-ouvert portent des
    /// numéros supérieurs et sont donc épargnés.
    pub fn sweep(&self, workdir: &std::path::Path) -> usize {
        let Some(oldest) = self.segments.first().and_then(|s| sequence(&s.path)) else {
            return 0;
        };
        let Ok(entries) = std::fs::read_dir(workdir) else {
            return 0;
        };

        let mut removed = 0;
        for path in entries.flatten().map(|e| e.path()) {
            let Some(number) = sequence(&path) else { continue };
            if number < oldest && std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
        removed
    }

    /// Efface tous les segments du disque.
    pub fn clear(&mut self) {
        for segment in self.segments.drain(..) {
            let _ = std::fs::remove_file(&segment.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(name: &str, seconds: f64, megabytes: u64) -> SegmentInfo {
        SegmentInfo {
            // Chemin fictif : `push` tente d'effacer les segments purgés, et
            // l'échec d'une suppression est volontairement ignoré.
            path: PathBuf::from(format!("segment_de_test_{name}.mp4")),
            bytes: megabytes * 1_048_576,
            duration_hns: (seconds * HNS_PER_SEC as f64) as i64,
        }
    }

    fn names(ring: &SegmentRing) -> Vec<String> {
        ring.segments()
            .iter()
            .map(|s| s.path.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn la_borne_de_duree_purge_les_plus_anciens() {
        let mut ring = SegmentRing::new(6.0, u64::MAX);
        for name in ["a", "b", "c", "d", "e"] {
            ring.push(segment(name, 2.0, 1));
        }
        // 6 s de budget pour des segments de 2 s : les trois derniers tiennent.
        assert_eq!(ring.segments().len(), 3);
        assert!(names(&ring)[0].contains("_c"));
    }

    #[test]
    fn la_borne_d_octets_purge_meme_si_la_duree_tient() {
        // Une heure de budget temporel : seule la taille peut déclencher la
        // purge. C'est le scénario réel, l'encodeur AMD ignorant tout plafond
        // de débit.
        let mut ring = SegmentRing::new(3600.0, 30 * 1_048_576);
        for name in ["a", "b", "c", "d"] {
            ring.push(segment(name, 2.0, 20));
        }
        assert_eq!(ring.segments().len(), 1);
        assert!(ring.bytes() <= 30 * 1_048_576);
    }

    #[test]
    fn un_segment_survit_toujours() {
        // Un plafond plus petit qu'un seul segment ne doit pas vider le buffer :
        // mieux vaut dépasser le budget que ne rien avoir à sauvegarder.
        let mut ring = SegmentRing::new(0.5, 1);
        ring.push(segment("unique", 2.0, 50));
        ring.push(segment("suivant", 2.0, 50));
        assert_eq!(ring.segments().len(), 1);
        assert!(names(&ring)[0].contains("suivant"));
    }

    #[test]
    fn les_totaux_suivent_le_contenu() {
        let mut ring = SegmentRing::new(3600.0, u64::MAX);
        ring.push(segment("a", 2.0, 3));
        ring.push(segment("b", 1.5, 4));
        assert_eq!(ring.bytes(), 7 * 1_048_576);
        assert_eq!(ring.duration_hns(), (3.5 * HNS_PER_SEC as f64) as i64);
    }

    #[test]
    fn le_balayage_efface_les_orphelins_anterieurs() {
        let dir = std::env::temp_dir().join(format!("smartclip_sweep_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Six fichiers sur le disque, mais l'anneau n'en retient que deux.
        for n in 0..6u32 {
            std::fs::write(dir.join(format!("seg{n:06}.mp4")), b"x").unwrap();
        }
        let mut ring = SegmentRing::new(3600.0, u64::MAX);
        for n in [3u32, 4] {
            ring.push(SegmentInfo {
                path: dir.join(format!("seg{n:06}.mp4")),
                bytes: 1,
                duration_hns: HNS_PER_SEC,
            });
        }

        assert_eq!(ring.sweep(&dir), 3);
        let restants: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        // Le numéro 5 est épargné : c'est le segment courant ou le pré-ouvert,
        // tous deux postérieurs au plus ancien de l'anneau.
        assert_eq!(restants.len(), 3);
        assert!(restants.iter().any(|n| n.contains("000005")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn vider_l_anneau_le_laisse_vide() {
        let mut ring = SegmentRing::new(3600.0, u64::MAX);
        ring.push(segment("a", 2.0, 1));
        ring.push(segment("b", 2.0, 1));
        ring.clear();
        assert!(ring.segments().is_empty());
        assert_eq!(ring.bytes(), 0);
        assert_eq!(ring.duration_hns(), 0);
    }
}
