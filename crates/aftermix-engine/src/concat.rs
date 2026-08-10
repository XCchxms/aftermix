//! Recollage des segments sans réencodage.
//!
//! Un `IMFSourceReader` auquel on ne configure aucun décodeur rend les
//! échantillons H.264 et AAC tels qu'ils sont sur le disque ; un `IMFSinkWriter`
//! qui déclare ces mêmes types en entrée et en sortie se contente de les
//! recopier. Aucun pixel n'est touché, aucune dépendance externe n'est
//! nécessaire : 20 s de vidéo se recollent en environ 400 ms.

use std::path::Path;

use anyhow::{Context, Result, bail};
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFSample, MF_SOURCE_READER_ANY_STREAM, MF_SOURCE_READERF_ENDOFSTREAM,
    MFCreateAttributes, MFCreateSinkWriterFromURL, MFCreateSourceReaderFromURL,
};
use windows::core::HSTRING;

use crate::segment::SegmentInfo;

/// Résultat d'un recollage.
pub struct ConcatOutcome {
    pub bytes: u64,
    pub samples: usize,
}

/// Concatène les segments dans l'ordre en recopiant les échantillons compressés.
pub fn concat(segments: &[SegmentInfo], output: &Path) -> Result<ConcatOutcome> {
    if segments.is_empty() {
        bail!("aucun segment à sauvegarder");
    }

    let mut attributes: Option<IMFAttributes> = None;
    unsafe { MFCreateAttributes(&mut attributes, 1)? };
    let attributes = attributes.context("attributs nuls")?;

    let writer = unsafe {
        MFCreateSinkWriterFromURL(&HSTRING::from(output.to_string_lossy().as_ref()), None, None)?
    };

    // Le premier segment fixe la structure de sortie : un flux par flux source,
    // aux types natifs, ce qui met le writer en simple recopie.
    let first = unsafe {
        MFCreateSourceReaderFromURL(
            &HSTRING::from(segments[0].path.to_string_lossy().as_ref()),
            &attributes,
        )?
    };
    let mut streams: Vec<(u32, u32)> = Vec::new(); // (index source, index writer)
    let mut index = 0u32;
    while let Ok(native) = unsafe { first.GetCurrentMediaType(index) } {
        let target = unsafe {
            first.SetStreamSelection(index, true)?;
            let target = writer.AddStream(&native)?;
            writer.SetInputMediaType(target, &native, None)?;
            target
        };
        streams.push((index, target));
        index += 1;
    }
    drop(first);

    if streams.is_empty() {
        bail!("le segment {} ne contient aucun flux", segments[0].path.display());
    }

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
        for (source, _) in &streams {
            unsafe { reader.SetStreamSelection(*source, true)? };
        }

        loop {
            let (mut actual, mut flags, mut timestamp) = (0u32, 0u32, 0i64);
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
            let Some((_, target)) = streams.iter().find(|(source, _)| *source == actual) else {
                continue;
            };
            unsafe {
                // Chaque segment repart de zéro : on le replace sur la timeline
                // globale en le décalant de la durée cumulée des précédents.
                sample.SetSampleTime(timestamp + offset_hns)?;
                writer.WriteSample(*target, &sample)?;
            }
            samples += 1;
        }
        offset_hns += segment.duration_hns;
    }

    unsafe { writer.Finalize()? };
    Ok(ConcatOutcome {
        bytes: std::fs::metadata(output).map(|m| m.len()).unwrap_or(0),
        samples,
    })
}
