//! Mixage des pistes et export final.
//!
//! C'est la raison d'être du produit : reprendre un clip déjà enregistré et
//! rééquilibrer chaque source indépendamment.
//!
//! La vidéo n'est **jamais** réencodée — ses échantillons sont recopiés tels
//! quels, comme au recollage. Seul l'audio est décodé, mixé selon les gains,
//! puis réencodé en une piste stéréo. Un export coûte donc le temps de traiter
//! l'audio, pas celui de réencoder l'image.
//!
//! Le mixage se fait par accumulation dans un tampon unique, chaque piste étant
//! placée d'après ses horodatages plutôt que par concaténation. Une piste qui
//! démarre en retard atterrit au bon endroit, et la mémoire ne dépend pas du
//! nombre de pistes : ~384 Ko par seconde de clip, quel qu'en soit le nombre.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFMediaType, IMFSample, IMFSourceReader,
    MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SINK_WRITER_DISABLE_THROTTLING,
    MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
    MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS,
    MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_PD_DURATION,
    MF_SOURCE_READER_ALL_STREAMS, MF_SOURCE_READER_MEDIASOURCE, MF_SOURCE_READERF_ENDOFSTREAM,
    MFAudioFormat_AAC, MFAudioFormat_Float, MFAudioFormat_PCM, MFCreateAttributes,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFCreateSinkWriterFromURL,
    MFCreateSourceReaderFromURL, MFMediaType_Audio, MFMediaType_Video,
    MF_MT_FRAME_SIZE, MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MFVideoFormat_RGB32,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Variant::VT_I8;
use windows::core::{GUID, HSTRING};

use smartclip_core::clock::HNS_PER_SEC;

use crate::{CHANNELS, SAMPLE_RATE};

/// Lectures vides consécutives tolérées avant de conclure au blocage.
///
/// Généreux : un fichier peut légitimement enchaîner quelques marqueurs de
/// temps. Mais des centaines d'affilée signalent que le lecteur ne progresse
/// plus, et il vaut mieux échouer avec un message clair qu'attendre sans fin.
const MAX_EMPTY_READS: u32 = 256;

/// Ce qu'un export a produit.
#[derive(Debug, Clone)]
pub struct MixOutcome {
    pub bytes: u64,
    pub seconds: f64,
    /// Amplitude maximale du mixage avant limitation.
    ///
    /// Au-delà de 1,0 le signal aurait saturé : l'interface doit le signaler et
    /// proposer de baisser les faders plutôt que de laisser passer une
    /// distorsion que l'utilisateur ne découvrirait qu'à la lecture.
    pub peak: f32,
    pub tracks_mixed: usize,
}

impl MixOutcome {
    pub fn clipped(&self) -> bool {
        self.peak > 1.0
    }
}

/// Décrit les pistes d'un clip, telles que l'éditeur doit les afficher.
#[derive(Debug, Clone)]
pub struct ClipInfo {
    pub video_stream: u32,
    pub audio_streams: Vec<u32>,
    pub duration_hns: i64,
}

fn open_reader(path: &Path) -> Result<IMFSourceReader> {
    let mut attributes: Option<IMFAttributes> = None;
    unsafe { MFCreateAttributes(&mut attributes, 1)? };
    let attributes = attributes.context("attributs nuls")?;
    Ok(unsafe {
        MFCreateSourceReaderFromURL(&HSTRING::from(path.to_string_lossy().as_ref()), &attributes)?
    })
}

/// Inventorie les flux d'un clip.
pub fn inspect(path: &Path) -> Result<ClipInfo> {
    let reader = open_reader(path)?;
    let mut video_stream = None;
    let mut audio_streams = Vec::new();
    let mut index = 0u32;

    while let Ok(media_type) = unsafe { reader.GetCurrentMediaType(index) } {
        let major = unsafe { media_type.GetGUID(&MF_MT_MAJOR_TYPE) }?;
        if major == MFMediaType_Video && video_stream.is_none() {
            video_stream = Some(index);
        } else if major == MFMediaType_Audio {
            audio_streams.push(index);
        }
        index += 1;
    }

    // `MF_PD_DURATION` arrive en VT_UI8 ; on lit le champ 64 bits directement,
    // windows-rs n'exposant pas de conversion pour ce type de PROPVARIANT.
    let duration_hns = unsafe {
        match reader.GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
        {
            Ok(value) => value.Anonymous.Anonymous.Anonymous.hVal,
            Err(_) => 0,
        }
    };

    Ok(ClipInfo {
        video_stream: video_stream.context("le clip ne contient aucune piste vidéo")?,
        audio_streams,
        duration_hns,
    })
}

/// Mixe les pistes selon `gains` et écrit le clip final.
///
/// `gains` suit l'ordre des pistes audio du fichier. Un gain nul coupe la piste,
/// et sa lecture est alors purement et simplement sautée — couper une source est
/// donc plus rapide que la garder.
pub fn mix_and_export(input: &Path, output: &Path, gains: &[f32]) -> Result<MixOutcome> {
    let info = inspect(input)?;
    if info.audio_streams.is_empty() {
        bail!("le clip ne contient aucune piste audio à mixer");
    }
    if gains.len() != info.audio_streams.len() {
        bail!(
            "{} gain(s) fourni(s) pour {} piste(s) audio",
            gains.len(),
            info.audio_streams.len()
        );
    }

    let frames = (info.duration_hns as i128 * SAMPLE_RATE as i128 / HNS_PER_SEC as i128) as usize;
    if frames == 0 {
        bail!("durée du clip inconnue ou nulle");
    }
    let mut mix = vec![0f32; frames * CHANNELS as usize];

    let mut tracks_mixed = 0;
    for (position, &gain) in gains.iter().enumerate() {
        if gain <= 0.0 {
            continue;
        }
        let started = std::time::Instant::now();
        accumulate_stream(input, info.audio_streams[position], gain, &mut mix)?;
        tracing::debug!(
            "piste {position} mixée en {:.0} ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
        tracks_mixed += 1;
    }

    let peak = mix.iter().fold(0f32, |acc, s| acc.max(s.abs()));
    let started = std::time::Instant::now();
    write_output(input, output, &info, &mix, peak)?;
    tracing::debug!(
        "écriture du clip final en {:.0} ms",
        started.elapsed().as_secs_f64() * 1000.0
    );

    Ok(MixOutcome {
        bytes: std::fs::metadata(output).map(|m| m.len()).unwrap_or(0),
        seconds: info.duration_hns as f64 / HNS_PER_SEC as f64,
        peak,
        tracks_mixed,
    })
}

/// Largeur des vignettes. La hauteur suit le rapport de l'image.
const THUMBNAIL_WIDTH: u32 = 480;

/// Extrait une image du clip et l'écrit en PNG.
///
/// Côté Rust plutôt que dans la vue : le protocole `asset:` n'est pas l'origine
/// de la page, ce qui « teinte » un canvas HTML et fait échouer sa lecture. Et
/// l'écrire sur disque la rend permanente, là où une extraction dans la vue
/// recommençait à chaque session.
///
/// L'image est prise un peu après le début — les toutes premières sont souvent
/// noires, le temps que l'encodeur se cale.
pub fn extract_thumbnail(input: &Path, output: &Path) -> Result<()> {
    let mut attributes: Option<IMFAttributes> = None;
    unsafe { MFCreateAttributes(&mut attributes, 1)? };
    let attributes = attributes.context("attributs nuls")?;
    unsafe {
        // Autorise le lecteur à insérer un convertisseur : c'est lui qui rend
        // du RGB depuis le NV12 natif du décodeur.
        attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
    }

    let reader = unsafe {
        MFCreateSourceReaderFromURL(&HSTRING::from(input.to_string_lossy().as_ref()), &attributes)?
    };
    let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

    unsafe {
        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        reader.SetStreamSelection(video, true)?;

        let target = MFCreateMediaType()?;
        target.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        target.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        reader
            .SetCurrentMediaType(video, None, &target)
            .context("le décodeur a refusé le format RGB")?;
    }

    // Dimensions réelles après conversion.
    let (width, height) = unsafe {
        let current = reader.GetCurrentMediaType(video)?;
        let packed = current.GetUINT64(&MF_MT_FRAME_SIZE)?;
        ((packed >> 32) as u32, (packed & 0xFFFF_FFFF) as u32)
    };
    if width == 0 || height == 0 {
        bail!("dimensions vidéo inconnues");
    }

    // Position de l'image : un quart du clip, borné à deux secondes. Assez tard
    // pour éviter le noir du début, assez tôt pour rester représentatif.
    let duration = inspect(input).map(|i| i.duration_hns).unwrap_or(0);
    let seek = (duration / 4).min(2 * HNS_PER_SEC);
    if seek > 0 {
        let mut position = PROPVARIANT::default();
        unsafe {
            let inner = &mut position.Anonymous.Anonymous;
            inner.vt = VT_I8;
            inner.Anonymous.hVal = seek;
            // Un positionnement refusé n'est pas fatal : on prendra la première
            // image venue plutôt que d'échouer.
            // GUID nul : le format de position par défaut, en unités de 100 ns.
            let _ = reader.SetCurrentPosition(&GUID::zeroed(), &position);
        }
    }

    // Lecture de la première image disponible.
    let mut empty_reads = 0u32;
    let sample = loop {
        let (mut actual, mut flags, mut timestamp) = (0u32, 0u32, 0i64);
        let mut sample: Option<IMFSample> = None;
        unsafe {
            reader.ReadSample(
                video,
                0,
                Some(&mut actual),
                Some(&mut flags),
                Some(&mut timestamp),
                Some(&mut sample),
            )?;
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            bail!("aucune image dans le clip");
        }
        match sample {
            Some(sample) => break sample,
            None => {
                empty_reads += 1;
                if empty_reads > MAX_EMPTY_READS {
                    bail!("aucune image exploitable");
                }
            }
        }
    };

    let pixels = unsafe {
        let buffer = sample.ConvertToContiguousBuffer()?;
        let mut data = std::ptr::null_mut();
        let mut length = 0u32;
        buffer.Lock(&mut data, None, Some(&mut length))?;
        let raw = std::slice::from_raw_parts(data, length as usize).to_vec();
        buffer.Unlock()?;
        raw
    };

    write_thumbnail(output, &pixels, width, height)
}

/// Réduit l'image et l'écrit en PNG.
fn write_thumbnail(output: &Path, pixels: &[u8], width: u32, height: u32) -> Result<()> {
    let target_width = THUMBNAIL_WIDTH.min(width);
    let target_height = (height * target_width / width).max(1);
    let mut rgba = Vec::with_capacity((target_width * target_height * 4) as usize);

    for y in 0..target_height {
        // RGB32 de Media Foundation est stocké de bas en haut : sans cette
        // inversion la vignette sort à l'envers.
        let source_y = height - 1 - (y * height / target_height).min(height - 1);
        for x in 0..target_width {
            let source_x = x * width / target_width;
            let offset = ((source_y * width + source_x) * 4) as usize;
            match pixels.get(offset..offset + 4) {
                // L'ordre est BGRA en mémoire, RGBA dans le PNG.
                Some(p) => rgba.extend_from_slice(&[p[2], p[1], p[0], 255]),
                None => rgba.extend_from_slice(&[0, 0, 0, 255]),
            }
        }
    }

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::io::BufWriter::new(std::fs::File::create(output)?);
    let mut encoder = png::Encoder::new(file, target_width, target_height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&rgba)?;
    Ok(())
}

/// Intervalles entre images vidéo consécutives, en millisecondes.
///
/// Sert à diagnostiquer une lecture saccadée : une timeline régulière donne des
/// écarts tous égaux à la période nominale, tandis qu'un fichier dont certaines
/// images ont été sautées présente des trous que les lecteurs rendent mal.
pub fn probe_video(input: &Path) -> Result<Vec<f64>> {
    let info = inspect(input)?;
    let reader = open_reader(input)?;
    unsafe {
        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        reader.SetStreamSelection(info.video_stream, true)?;
    }

    let mut gaps = Vec::new();
    let mut previous: Option<i64> = None;
    let mut empty_reads = 0u32;
    loop {
        let (mut actual, mut flags, mut timestamp) = (0u32, 0u32, 0i64);
        let mut sample: Option<IMFSample> = None;
        unsafe {
            reader.ReadSample(
                info.video_stream,
                0,
                Some(&mut actual),
                Some(&mut flags),
                Some(&mut timestamp),
                Some(&mut sample),
            )?;
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            break;
        }
        if sample.is_none() {
            empty_reads += 1;
            if empty_reads > MAX_EMPTY_READS {
                break;
            }
            continue;
        }
        empty_reads = 0;
        if let Some(before) = previous {
            gaps.push((timestamp - before) as f64 / 10_000.0);
        }
        previous = Some(timestamp);
    }
    Ok(gaps)
}

/// Extrait chaque piste audio dans un WAV séparé, pour l'écoute en direct.
///
/// Le webview ne sait lire qu'une seule piste d'un MP4 multi-pistes : il est
/// donc impossible de prévisualiser un réglage sans sortir les pistes du
/// conteneur. Chacune devient un WAV que la vue charge dans un `AudioBuffer` et
/// pilote par son propre gain — l'utilisateur entend le résultat sans avoir à
/// exporter.
///
/// Les fichiers sont écrits en PCM 16 bits : deux fois plus légers que du
/// flottant, pour une différence inaudible à la prévisualisation.
pub fn extract_tracks(input: &Path, directory: &Path) -> Result<Vec<PathBuf>> {
    let info = inspect(input)?;
    std::fs::create_dir_all(directory)?;

    let frames = (info.duration_hns as i128 * SAMPLE_RATE as i128 / HNS_PER_SEC as i128) as usize;
    if frames == 0 {
        bail!("durée du clip inconnue ou nulle");
    }

    let mut written = Vec::with_capacity(info.audio_streams.len());
    for (position, &stream) in info.audio_streams.iter().enumerate() {
        let mut samples = vec![0f32; frames * CHANNELS as usize];
        accumulate_stream(input, stream, 1.0, &mut samples)?;

        let path = directory.join(format!("track{position}.wav"));
        write_wav(&path, &samples)?;
        written.push(path);
    }
    Ok(written)
}

/// Écrit un WAV PCM 16 bits stéréo.
fn write_wav(path: &Path, samples: &[f32]) -> Result<()> {
    use std::io::Write;

    let data_bytes = (samples.len() * 2) as u32;
    let block_align = CHANNELS * 2;
    let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);

    file.write_all(b"RIFF")?;
    // Charge utile : "WAVE" (4) + fmt de 16 octets (8+16) + en-tête data (8).
    file.write_all(&(36 + data_bytes).to_le_bytes())?;
    file.write_all(b"WAVE")?;
    file.write_all(b"fmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?; // PCM
    file.write_all(&CHANNELS.to_le_bytes())?;
    file.write_all(&SAMPLE_RATE.to_le_bytes())?;
    file.write_all(&(SAMPLE_RATE * block_align as u32).to_le_bytes())?;
    file.write_all(&block_align.to_le_bytes())?;
    file.write_all(&16u16.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&data_bytes.to_le_bytes())?;

    for &sample in samples {
        let value = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
        file.write_all(&value.to_le_bytes())?;
    }
    file.flush()?;
    Ok(())
}

/// Décode une piste et l'ajoute au tampon de mixage, à la position dictée par
/// ses horodatages.
fn accumulate_stream(input: &Path, stream: u32, gain: f32, mix: &mut [f32]) -> Result<()> {
    let reader = open_reader(input)?;
    unsafe {
        // Ne lire que la piste voulue : tout le reste est désélectionné, ce qui
        // évite au lecteur de décoder ce dont on n'a pas besoin.
        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        reader.SetStreamSelection(stream, true)?;

        // Demander du flottant place la conversion de format à la charge de
        // Media Foundation, y compris si la piste était mono ou à une autre
        // fréquence.
        let target = MFCreateMediaType()?;
        target.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        target.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float)?;
        target.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 32)?;
        target.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)?;
        target.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS as u32)?;
        reader
            .SetCurrentMediaType(stream, None, &target)
            .context("le décodeur a refusé le format flottant")?;

        let mut empty_reads = 0u32;
        // Échantillons déjà déposés pour cette piste, qui donnent la position
        // du paquet suivant.
        let mut written = 0usize;
        loop {
            let (mut actual, mut flags, mut timestamp) = (0u32, 0u32, 0i64);
            let mut sample: Option<IMFSample> = None;
            reader.ReadSample(
                stream,
                0,
                Some(&mut actual),
                Some(&mut flags),
                Some(&mut timestamp),
                Some(&mut sample),
            )?;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
            // Un échantillon vide sans fin de flux est légitime — marqueur de
            // temps, trou dans la piste — mais s'il se répète, `continue`
            // boucle sans fin. Un export s'est ainsi figé plus de six minutes
            // sur un clip d'une minute.
            let Some(sample) = sample else {
                empty_reads += 1;
                if empty_reads > MAX_EMPTY_READS {
                    bail!("lecture bloquée sur la piste {stream} : {empty_reads} paquets vides");
                }
                continue;
            };
            empty_reads = 0;

            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut data = std::ptr::null_mut();
            let mut length = 0u32;
            buffer.Lock(&mut data, None, Some(&mut length))?;

            let samples = std::slice::from_raw_parts(data as *const f32, length as usize / 4);

            // Les paquets d'une même piste se suivent, ils ne se placent pas
            // par horodatage.
            //
            // Positionner chaque paquet d'après son timestamp semblait plus
            // fidèle, mais ces horodatages sont arrondis : deux paquets
            // consécutifs se chevauchent de quelques échantillons, et comme
            // l'accumulation est une addition — nécessaire pour superposer les
            // pistes — le signal s'ajoutait à lui-même sur la zone commune.
            // D'où un craquement tous les 21 ms, soit un grésillement continu.
            //
            // Écrits à la suite, les paquets d'une piste forment un signal
            // exact. Le décalage entre pistes reste correct : elles démarrent
            // toutes au début du clip.
            for (i, &value) in samples.iter().enumerate() {
                match mix.get_mut(written + i) {
                    Some(slot) => *slot += value * gain,
                    // Au-delà de la durée annoncée : rien à faire, le tampon
                    // fait foi.
                    None => break,
                }
            }
            written += samples.len();

            buffer.Unlock()?;
        }
    }
    Ok(())
}

/// Écrit le clip final : vidéo recopiée, audio mixé réencodé.
fn write_output(
    input: &Path,
    output: &Path,
    info: &ClipInfo,
    mix: &[f32],
    peak: f32,
) -> Result<()> {
    let reader = open_reader(input)?;
    unsafe {
        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        reader.SetStreamSelection(info.video_stream, true)?;
    }
    let video_type: IMFMediaType = unsafe { reader.GetCurrentMediaType(info.video_stream)? };

    // Le writer d'export est configuré pour le débit, pas pour le direct.
    //
    // Sans attributs, l'écriture d'un clip de 54 s prenait 3 min 21. La
    // régulation n'a ici aucun intérêt — le contenu est borné, entièrement en
    // mémoire, et l'entrelacement garantit qu'aucun flux ne prend d'avance :
    // c'est exactement le cas où la désactiver est légitime, à l'inverse de la
    // capture continue où elle protège d'une accumulation sans fin.
    let mut attributes: Option<IMFAttributes> = None;
    unsafe { MFCreateAttributes(&mut attributes, 2)? };
    let attributes = attributes.context("attributs nuls")?;
    unsafe {
        attributes.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
        attributes.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
    }

    let writer = unsafe {
        MFCreateSinkWriterFromURL(
            &HSTRING::from(output.to_string_lossy().as_ref()),
            None,
            &attributes,
        )?
    };

    // Vidéo : type natif en entrée comme en sortie, donc simple recopie.
    let video_out = unsafe { writer.AddStream(&video_type)? };
    unsafe { writer.SetInputMediaType(video_out, &video_type, None)? };

    let audio_out = unsafe {
        let out = MFCreateMediaType()?;
        out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        out.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
        out.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        out.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)?;
        out.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS as u32)?;
        // 192 kbps : le clip final est destiné au partage, pas à l'édition.
        out.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 24_000)?;
        let stream = writer.AddStream(&out)?;

        let inp = MFCreateMediaType()?;
        inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        inp.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
        inp.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        inp.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)?;
        inp.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS as u32)?;
        inp.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, (CHANNELS * 2) as u32)?;
        writer.SetInputMediaType(stream, &inp, None)?;
        stream
    };

    unsafe { writer.BeginWriting()? };

    // L'audio et la vidéo doivent être **entrelacés**.
    //
    // Écrire soixante secondes d'audio avant la première image demandait au
    // muxeur de retenir tout ce flux en attendant que la vidéo le rattrape ;
    // passé une certaine avance, sa régulation bloque `WriteSample` et l'export
    // ne se termine jamais. Un clip de 14 s passait, un clip de 60 s figeait le
    // processus plus de six minutes.
    //
    // On avance donc la vidéo image par image, en intercalant les blocs audio
    // dès que leur horodatage est rattrapé.
    const BLOCK_FRAMES: usize = SAMPLE_RATE as usize / 10;
    let channels = CHANNELS as usize;
    // Le gain cumulé peut dépasser 1,0 ; on limite au lieu de normaliser, pour
    // que le réglage des faders reste prévisible d'un export à l'autre.
    let limiter = if peak > 1.0 { 1.0 / peak } else { 1.0 };

    let blocks: Vec<&[f32]> = mix.chunks(BLOCK_FRAMES * channels).collect();
    let block_hns = BLOCK_FRAMES as i64 * HNS_PER_SEC / SAMPLE_RATE as i64;
    let mut next_block = 0usize;

    // Écrit les blocs audio dont l'horodatage précède `until_hns`.
    let flush_audio = |next_block: &mut usize, until_hns: i64| -> Result<()> {
        while *next_block < blocks.len() && (*next_block as i64) * block_hns <= until_hns {
            let chunk = blocks[*next_block];
            let pcm: Vec<i16> = chunk
                .iter()
                .map(|&s| ((s * limiter).clamp(-1.0, 1.0) * 32767.0) as i16)
                .collect();
            let bytes = std::mem::size_of_val(&pcm[..]);
            unsafe {
                let buffer = MFCreateMemoryBuffer(bytes as u32)?;
                let mut dst = std::ptr::null_mut();
                buffer.Lock(&mut dst, None, None)?;
                std::ptr::copy_nonoverlapping(pcm.as_ptr() as *const u8, dst, bytes);
                buffer.Unlock()?;
                buffer.SetCurrentLength(bytes as u32)?;

                let sample = MFCreateSample()?;
                sample.AddBuffer(&buffer)?;
                sample.SetSampleTime(*next_block as i64 * block_hns)?;
                let frames = chunk.len() / channels;
                sample.SetSampleDuration(frames as i64 * HNS_PER_SEC / SAMPLE_RATE as i64)?;
                writer.WriteSample(audio_out, &sample)?;
            }
            *next_block += 1;
        }
        Ok(())
    };

    // Vidéo : recopie intégrale, sans décodage, l'audio suivant sa progression.
    let mut empty_reads = 0u32;
    loop {
        let (mut actual, mut flags, mut timestamp) = (0u32, 0u32, 0i64);
        let mut sample: Option<IMFSample> = None;
        unsafe {
            reader.ReadSample(
                info.video_stream,
                0,
                Some(&mut actual),
                Some(&mut flags),
                Some(&mut timestamp),
                Some(&mut sample),
            )?;
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            break;
        }
        let Some(sample) = sample else {
            empty_reads += 1;
            if empty_reads > MAX_EMPTY_READS {
                bail!("lecture vidéo bloquée : {empty_reads} paquets vides d'affilée");
            }
            continue;
        };
        empty_reads = 0;
        // L'audio est amené au niveau de l'image courante avant de l'écrire :
        // aucun flux ne prend jamais plus d'un bloc d'avance sur l'autre.
        flush_audio(&mut next_block, timestamp)?;
        unsafe { writer.WriteSample(video_out, &sample)? };
    }

    // Solde de l'audio : la piste mixée peut dépasser la dernière image.
    flush_audio(&mut next_block, i64::MAX)?;

    unsafe { writer.Finalize()? };
    Ok(())
}
