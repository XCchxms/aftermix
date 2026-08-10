//! Déclenchement à la voix.
//!
//! Une phrase d'activation — « ok aftermix », ou ce que l'utilisateur veut —
//! sauvegarde un clip sans lâcher la souris ni le clavier. C'est le complément
//! naturel du raccourci : en pleine action, une main occupée est une main de
//! moins pour appuyer sur trois touches.
//!
//! Le moteur est celui de Windows, contraint à une **liste de phrases** au lieu
//! d'une dictée libre. Cette contrainte fait tout : le système ne cherche pas à
//! transcrire ce qui est dit, il le compare à une poignée de phrases attendues.
//! La reconnaissance tourne hors ligne, ne consomme presque rien, et le contenu
//! de ce qui est dit ne quitte jamais la machine.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows::Foundation::TypedEventHandler;
use windows::Media::SpeechRecognition::{
    SpeechContinuousRecognitionResultGeneratedEventArgs, SpeechContinuousRecognitionSession,
    SpeechRecognitionConfidence, SpeechRecognitionListConstraint, SpeechRecognizer,
};
use windows::core::HSTRING;
use windows_collections::IIterable;
use windows_future::AsyncStatus;

/// Longueur minimale d'une phrase d'activation.
///
/// Une phrase courte se confond avec la conversation ordinaire et déclenche à
/// tout propos — en vocal ou en pleine partie, on parle beaucoup.
const MIN_PHRASE_LEN: usize = 6;

/// Délai maximal d'attente d'une opération WinRT.
const ASYNC_TIMEOUT: Duration = Duration::from_secs(10);

/// Attend une opération asynchrone WinRT sans exécuteur.
///
/// Les opérations de windows-future implémentent `IntoFuture`, ce qui suppose un
/// runtime async dont le projet n'a pas besoin par ailleurs. Interroger `Status`
/// est plus simple et suffit ici : ces opérations durent quelques centaines de
/// millisecondes, une fois au démarrage de l'écoute.
macro_rules! wait_async {
    ($operation:expr, $label:literal) => {{
        let operation = $operation;
        let deadline = Instant::now() + ASYNC_TIMEOUT;
        loop {
            match operation.Status()? {
                AsyncStatus::Completed => break operation.GetResults(),
                AsyncStatus::Error | AsyncStatus::Canceled => {
                    bail!(concat!($label, " a échoué"))
                }
                _ if Instant::now() > deadline => {
                    bail!(concat!("délai dépassé sur ", $label))
                }
                _ => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }};
}

/// Ne retient que les reconnaissances sûres.
///
/// Windows classe chaque résultat en quatre niveaux. Écarter les deux plus
/// faibles évite les rapprochements hasardeux : un déclenchement intempestif
/// coûte plus cher qu'une phrase à répéter.
fn is_confident(confidence: SpeechRecognitionConfidence) -> bool {
    confidence == SpeechRecognitionConfidence::High
        || confidence == SpeechRecognitionConfidence::Medium
}

/// Écoute d'une phrase d'activation. L'écoute cesse avec cette valeur.
pub struct VoiceTrigger {
    session: SpeechContinuousRecognitionSession,
    /// Coupe le rappel avant même l'arrêt effectif : la session peut livrer un
    /// dernier résultat pendant qu'elle s'arrête.
    stopped: Arc<AtomicBool>,
    _recognizer: SpeechRecognizer,
}

impl VoiceTrigger {
    /// Démarre l'écoute et appelle `on_trigger` à chaque phrase reconnue.
    ///
    /// Échoue si la reconnaissance vocale de Windows est indisponible — pack de
    /// langue absent, micro inaccessible, fonctionnalité désactivée. C'est un
    /// cas courant et sans gravité : l'appelant se rabat sur le raccourci
    /// clavier, à condition de le dire à l'utilisateur.
    pub fn start<F>(phrase: &str, on_trigger: F) -> Result<Self>
    where
        F: Fn() + Send + 'static,
    {
        let phrase = phrase.trim();
        if phrase.chars().count() < MIN_PHRASE_LEN {
            bail!("la phrase d'activation doit faire au moins {MIN_PHRASE_LEN} caractères");
        }

        let recognizer = SpeechRecognizer::new()
            .context("la reconnaissance vocale de Windows est indisponible")?;

        // `IIterable` se construit directement depuis un `Vec` — inutile de
        // passer par une collection WinRT mutable.
        let commands: IIterable<HSTRING> = vec![HSTRING::from(phrase)].into();
        let constraint = SpeechRecognitionListConstraint::Create(&commands)
            .context("phrase d'activation refusée")?;
        recognizer.Constraints()?.Append(&constraint)?;

        let compilation = wait_async!(
            recognizer.CompileConstraintsAsync()?,
            "la compilation de la phrase d'activation"
        )?;
        let status = compilation.Status()?;
        if status.0 != 0 {
            bail!("Windows a refusé la phrase d'activation (statut {})", status.0);
        }

        let session = recognizer.ContinuousRecognitionSession()?;
        let stopped = Arc::new(AtomicBool::new(false));

        let handler = {
            let stopped = Arc::clone(&stopped);
            TypedEventHandler::new(
                move |_, args: windows_core::Ref<'_, SpeechContinuousRecognitionResultGeneratedEventArgs>| {
                    if stopped.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                    if let Some(args) = args.as_ref() {
                        if let Ok(result) = args.Result() {
                            if result.Confidence().map(is_confident).unwrap_or(false) {
                                tracing::info!("phrase d'activation reconnue");
                                on_trigger();
                            } else {
                                tracing::debug!("phrase entendue, confiance insuffisante");
                            }
                        }
                    }
                    Ok(())
                },
            )
        };
        session.ResultGenerated(&handler)?;
        wait_async!(session.StartAsync()?, "le démarrage de l'écoute")?;

        tracing::info!("écoute vocale active sur « {phrase} »");
        Ok(Self {
            session,
            stopped,
            _recognizer: recognizer,
        })
    }
}

impl Drop for VoiceTrigger {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Ok(operation) = self.session.StopAsync() {
            // L'arrêt n'est pas attendu : il peut traîner, et plus rien ne
            // dépend de son issue une fois le rappel neutralisé.
            let _ = operation.Status();
        }
    }
}

// La session n'est jamais partagée entre threads : elle est transférée entière
// lorsque le moteur change de phrase d'activation.
unsafe impl Send for VoiceTrigger {}
