//! Bibliothèque de clips et métadonnées.
//!
//! Le MP4 ne sait pas nommer ses pistes d'une façon que Media Foundation
//! permette d'écrire et de relire simplement. Sans nom, le mixeur afficherait
//! « piste 0, piste 1 » là où l'utilisateur attend « Jeu, Discord » — ce qui
//! viderait la fonction principale de son sens.
//!
//! Chaque clip est donc accompagné d'un fichier `.json` de même nom. Le MP4
//! reste lisible et partageable seul ; le sidecar n'ajoute que ce dont
//! l'éditeur a besoin. S'il manque, la bibliothèque retombe sur des noms
//! génériques plutôt que d'ignorer le clip.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Métadonnées écrites à côté du clip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipMeta {
    /// Libellés des pistes, dans l'ordre du fichier.
    pub tracks: Vec<String>,
    pub seconds: f64,
    /// Date de création, en secondes depuis l'époque Unix.
    pub created: u64,
}

impl ClipMeta {
    /// Chemin du sidecar associé à un clip.
    pub fn sidecar_path(clip: &Path) -> PathBuf {
        clip.with_extension("json")
    }

    /// Chemin de la vignette associée à un clip.
    pub fn thumbnail_path(clip: &Path) -> PathBuf {
        clip.with_extension("png")
    }

    pub fn write(&self, clip: &Path) -> Result<()> {
        let path = Self::sidecar_path(clip);
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json).with_context(|| format!("écriture de {}", path.display()))
    }

    pub fn read(clip: &Path) -> Option<Self> {
        let raw = std::fs::read_to_string(Self::sidecar_path(clip)).ok()?;
        serde_json::from_str(&raw).ok()
    }
}

/// Un clip tel que la bibliothèque l'expose.
#[derive(Debug, Clone)]
pub struct Clip {
    pub path: PathBuf,
    /// Nom de fichier sans extension, ce que l'interface affiche.
    pub name: String,
    pub bytes: u64,
    pub seconds: f64,
    pub tracks: Vec<String>,
    pub created: u64,
    /// Vignette du clip, si elle a pu être extraite.
    pub thumbnail: Option<PathBuf>,
    /// Vrai si les métadonnées manquaient et que les noms sont des replis.
    pub metadata_missing: bool,
}

/// Liste les clips d'un dossier, du plus récent au plus ancien.
///
/// Un dossier absent n'est pas une erreur : c'est l'état normal avant la
/// première sauvegarde, et l'interface doit pouvoir afficher une bibliothèque
/// vide sans traiter le cas comme un échec.
pub fn scan(directory: &Path) -> Result<Vec<Clip>> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Ok(Vec::new());
    };

    let mut clips: Vec<Clip> = entries
        .flatten()
        .filter(|entry| {
            entry
                .path()
                .extension()
                .map(|e| e.eq_ignore_ascii_case("mp4"))
                .unwrap_or(false)
        })
        .filter_map(|entry| describe(&entry.path()).ok())
        .collect();

    clips.sort_by(|a, b| b.created.cmp(&a.created));
    Ok(clips)
}

/// Décrit un clip à partir de son chemin et de son sidecar.
pub fn describe(path: &Path) -> Result<Clip> {
    let metadata = std::fs::metadata(path)?;
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "clip".to_string());

    let fallback_created = metadata
        .created()
        .or_else(|_| metadata.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let meta = ClipMeta::read(path);
    let metadata_missing = meta.is_none();
    let meta = meta.unwrap_or_else(|| ClipMeta {
        tracks: Vec::new(),
        seconds: 0.0,
        created: fallback_created,
    });

    let thumbnail = ClipMeta::thumbnail_path(path);
    Ok(Clip {
        path: path.to_path_buf(),
        name,
        thumbnail: thumbnail.exists().then_some(thumbnail),
        bytes: metadata.len(),
        seconds: meta.seconds,
        tracks: meta.tracks,
        created: meta.created,
        metadata_missing,
    })
}

/// Supprime un clip et son sidecar.
///
/// L'absence du sidecar n'est pas signalée : il peut ne jamais avoir existé, et
/// l'utilisateur a demandé la suppression du clip, pas celle d'un fichier
/// annexe dont il ignore l'existence.
pub fn delete(clip: &Path) -> Result<()> {
    std::fs::remove_file(clip).with_context(|| format!("suppression de {}", clip.display()))?;
    let _ = std::fs::remove_file(ClipMeta::sidecar_path(clip));
    let _ = std::fs::remove_file(ClipMeta::thumbnail_path(clip));
    Ok(())
}

/// Horodatage courant, en secondes depuis l'époque Unix.
pub fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dossier temporaire supprimé à la fin du test.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!("smartclip_test_{tag}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn make_clip(dir: &Path, name: &str, meta: Option<ClipMeta>) -> PathBuf {
        let path = dir.join(format!("{name}.mp4"));
        std::fs::write(&path, b"pas un vrai mp4").unwrap();
        if let Some(meta) = meta {
            meta.write(&path).unwrap();
        }
        path
    }

    #[test]
    fn un_dossier_absent_donne_une_bibliotheque_vide() {
        let missing = std::env::temp_dir().join("smartclip_dossier_qui_nexiste_pas");
        assert!(scan(&missing).unwrap().is_empty());
    }

    #[test]
    fn les_pistes_survivent_a_l_aller_retour() {
        let dir = TempDir::new("meta");
        let meta = ClipMeta {
            tracks: vec!["Jeu".into(), "Discord".into(), "Micro".into()],
            seconds: 42.5,
            created: 1_700_000_000,
        };
        let clip = make_clip(&dir.0, "avec_meta", Some(meta));

        let described = describe(&clip).unwrap();
        assert_eq!(described.tracks, ["Jeu", "Discord", "Micro"]);
        assert_eq!(described.seconds, 42.5);
        assert!(!described.metadata_missing);
    }

    #[test]
    fn un_clip_sans_sidecar_reste_listable() {
        let dir = TempDir::new("orphelin");
        let clip = make_clip(&dir.0, "sans_meta", None);

        let described = describe(&clip).unwrap();
        assert!(described.metadata_missing);
        assert!(described.tracks.is_empty());
        // Le repli sur la date du fichier évite un clip daté de 1970 en tête de
        // liste.
        assert!(described.created > 0);
    }

    #[test]
    fn les_clips_sortent_du_plus_recent_au_plus_ancien() {
        let dir = TempDir::new("tri");
        for (name, created) in [("vieux", 1_000), ("recent", 3_000), ("moyen", 2_000)] {
            make_clip(
                &dir.0,
                name,
                Some(ClipMeta {
                    tracks: vec![],
                    seconds: 1.0,
                    created,
                }),
            );
        }
        let names: Vec<String> = scan(&dir.0).unwrap().into_iter().map(|c| c.name).collect();
        assert_eq!(names, ["recent", "moyen", "vieux"]);
    }

    #[test]
    fn supprimer_emporte_le_sidecar() {
        let dir = TempDir::new("suppr");
        let clip = make_clip(
            &dir.0,
            "a_supprimer",
            Some(ClipMeta {
                tracks: vec!["Jeu".into()],
                seconds: 1.0,
                created: 1,
            }),
        );
        let sidecar = ClipMeta::sidecar_path(&clip);
        assert!(sidecar.exists());

        delete(&clip).unwrap();
        assert!(!clip.exists());
        assert!(!sidecar.exists());
    }

    #[test]
    fn seuls_les_mp4_sont_listes() {
        let dir = TempDir::new("filtre");
        make_clip(&dir.0, "vrai", None);
        std::fs::write(dir.0.join("notes.txt"), b"bruit").unwrap();
        std::fs::write(dir.0.join("autre.mkv"), b"bruit").unwrap();

        let clips = scan(&dir.0).unwrap();
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].name, "vrai");
    }
}
