// Écoute en direct du mixage.
//
// La vidéo joue muette et sert d'horloge maître ; chaque piste est un
// AudioBufferSourceNode branché sur son propre GainNode. Bouger un fader agit
// donc immédiatement, sans réencoder quoi que ce soit.
//
// Pourquoi pas cinq éléments <audio> en parallèle : leurs horloges sont
// indépendantes de celle de la vidéo et dérivent en quelques dizaines de
// secondes. Web Audio place au contraire chaque source sur une position
// explicite de la timeline, et permet de la corriger.

/// Seuil de resynchronisation.
///
/// Chaque resynchronisation relance toutes les sources, ce qui s'entend malgré
/// le fondu : mieux vaut tolérer un décalage discret que corriger sans cesse.
/// 120 ms restent imperceptibles sur un clip de jeu, là où 40 ms déclenchaient
/// une correction toutes les quelques secondes.
const RESYNC_THRESHOLD = 0.12;

/// En dessous de ce niveau, une piste est considérée comme muette.
const SILENCE_PEAK = 0.005;

/// Au-delà de cette corrélation, deux pistes portent le même son.
///
/// Volontairement élevé : deux sources qui se ressemblent vaguement — une
/// musique et un jeu qui joue la même musique — ne doivent pas déclencher
/// l'alerte. Un retour de micro, lui, dépasse largement ce seuil.
const DUPLICATE_THRESHOLD = 0.75;

/// Corrélation normalisée entre deux tampons, sur un échantillonnage régulier.
///
/// Ce n'est pas une analyse fine : on cherche à savoir si deux pistes portent le
/// même signal, pas à mesurer un décalage. Un pas large garde le calcul
/// instantané même sur cinq minutes de clip.
function correlation(first, second) {
  const a = first.getChannelData(0);
  const b = second.getChannelData(0);
  const length = Math.min(a.length, b.length);
  const step = Math.max(1, Math.floor(length / 4000));

  let dot = 0;
  let normA = 0;
  let normB = 0;
  for (let i = 0; i < length; i += step) {
    dot += a[i] * b[i];
    normA += a[i] * a[i];
    normB += b[i] * b[i];
  }
  if (normA === 0 || normB === 0) return 0;
  return Math.abs(dot) / Math.sqrt(normA * normB);
}

/// Amplitude maximale d'un tampon, échantillonnée grossièrement.
///
/// Sert à repérer les pistes vides : une application ouverte mais silencieuse
/// produit un flux complet de zéros, indiscernable d'une capture ratée tant
/// qu'on ne regarde pas le signal. Un pas large suffit — on cherche à savoir
/// s'il se passe quelque chose, pas à mesurer précisément.
function peakOf(buffer) {
  const data = buffer.getChannelData(0);
  let peak = 0;
  for (let i = 0; i < data.length; i += 512) {
    const value = Math.abs(data[i]);
    if (value > peak) peak = value;
  }
  return peak;
}

export class Preview {
  constructor(video) {
    this.video = video;
    this.context = null;
    this.tracks = new Map(); // index → { buffer, gainNode, source }
    this.playing = false;
    this.startedAt = 0;   // ctx.currentTime au démarrage
    this.startOffset = 0; // position dans le clip au démarrage
  }

  get ready() {
    return this.tracks.size > 0;
  }

  /// Paires de pistes qui portent visiblement le même signal.
  ///
  /// Cas très courant : Discord réémet le micro — retour local, ou passage par
  /// un transformateur de voix — si bien que cumuler les deux pistes fait
  /// entendre la voix en double. Le produit ne peut pas l'empêcher, mais le
  /// signaler évite à l'utilisateur de chercher seul pourquoi il s'entend deux
  /// fois.
  duplicatePairs() {
    const audible = [...this.tracks.entries()].filter(
      ([, track]) => track.peak >= SILENCE_PEAK,
    );
    const pairs = [];
    for (let i = 0; i < audible.length; i++) {
      for (let j = i + 1; j < audible.length; j++) {
        const score = correlation(audible[i][1].buffer, audible[j][1].buffer);
        if (score > DUPLICATE_THRESHOLD) {
          pairs.push({ a: audible[i][0], b: audible[j][0], score });
        }
      }
    }
    return pairs;
  }

  /// Indices des pistes qui ne contiennent aucun son.
  silentTracks() {
    return [...this.tracks.entries()]
      .filter(([, track]) => track.peak < SILENCE_PEAK)
      .map(([index]) => index);
  }

  /// Charge les pistes extraites. Les WAV sont décodés une fois pour toutes.
  async load(descriptors, toUrl) {
    this.unload();
    // Aucune fréquence n'est imposée au contexte.
    //
    // Exiger 48 kHz échoue quand la sortie de la machine tourne à une autre
    // fréquence — 44,1 kHz est courant — et faisait échouer toute l'écoute.
    // `decodeAudioData` rééchantillonne de lui-même vers celle du contexte.
    this.context = this.context ?? new AudioContext();

    // Chaque étape est nommée : un échec ici laissait l'éditeur muet sans que
    // rien n'indique laquelle avait cédé — lecture du fichier, autorisation
    // d'accès ou décodage.
    const decoded = await Promise.all(
      descriptors.map(async (track) => {
        let response;
        try {
          response = await fetch(toUrl(track.path));
        } catch (cause) {
          throw new Error(`accès refusé à ${track.label} (${cause})`);
        }
        if (!response.ok) {
          throw new Error(`piste ${track.label} illisible (HTTP ${response.status})`);
        }
        const bytes = await response.arrayBuffer();
        try {
          return { index: track.index, buffer: await this.context.decodeAudioData(bytes) };
        } catch (cause) {
          throw new Error(`décodage impossible pour ${track.label} (${cause})`);
        }
      }),
    );

    for (const { index, buffer } of decoded) {
      const gainNode = this.context.createGain();
      gainNode.connect(this.context.destination);
      this.tracks.set(index, { buffer, gainNode, source: null, peak: peakOf(buffer) });
    }

    // La vidéo porte sa propre piste audio, que le webview lirait en plus du
    // mixage : on la coupe une fois les pistes prêtes.
    this.video.muted = true;
  }

  setGain(index, gain) {
    const track = this.tracks.get(index);
    if (!track) return;
    const wasSilent = track.gainNode.gain.value <= 0;
    // Une rampe très courte évite le claquement d'un saut de gain brutal.
    track.gainNode.gain.setTargetAtTime(gain, this.context.currentTime, 0.01);
    track.gainNode.gain.value = gain;

    // Une piste rallumée n'a pas de source en cours — elle n'en avait pas
    // besoin tant qu'elle était à zéro. On relance l'ensemble pour la remettre
    // en jeu, calée sur les autres.
    if (wasSilent && gain > 0 && this.playing && !track.source) {
      this.start();
    }
  }

  /// (Re)démarre toutes les pistes à la position courante de la vidéo.
  start() {
    if (!this.ready) return;
    this.stopSources();
    if (this.context.state === "suspended") this.context.resume();

    const offset = this.video.currentTime;
    this.startedAt = this.context.currentTime;
    this.startOffset = offset;

    const now = this.context.currentTime;
    for (const track of this.tracks.values()) {
      // Les pistes muettes ne sont pas jouées du tout.
      //
      // Chaque source active occupe le thread audio en temps réel, pendant que
      // le webview décode aussi la vidéo. Au-delà de quelques sources, il ne
      // tient plus la cadence et la sortie craque. Une piste sans contenu, ou
      // dont le fader est à zéro, n'apporte rien : ne pas la créer réduit
      // d'autant la charge — souvent de moitié, la moitié des emplacements
      // étant vides.
      if (track.peak < SILENCE_PEAK || track.gainNode.gain.value <= 0) {
        continue;
      }
      const source = this.context.createBufferSource();
      source.buffer = track.buffer;
      source.connect(track.gainNode);

      // Fondu d'entrée de 12 ms.
      //
      // Démarrer une source en pleine forme d'onde produit un clic. Comme la
      // resynchronisation relance toutes les sources — jusqu'à deux fois par
      // seconde en cas de dérive —, ces clics se succédaient et s'entendaient
      // comme un grésillement continu.
      const target = track.gainNode.gain.value;
      track.gainNode.gain.cancelScheduledValues(now);
      track.gainNode.gain.setValueAtTime(0, now);
      track.gainNode.gain.linearRampToValueAtTime(target, now + 0.012);

      // Toutes les sources partent au même instant, avec le même décalage :
      // c'est ce qui garantit qu'elles restent entre elles parfaitement calées.
      source.start(0, Math.min(offset, track.buffer.duration));
      track.source = source;
    }
    this.playing = true;
  }

  stop() {
    this.stopSources();
    this.playing = false;
  }

  stopSources() {
    for (const track of this.tracks.values()) {
      if (track.source) {
        try { track.source.stop(); } catch { /* déjà arrêtée */ }
        track.source.disconnect();
        track.source = null;
      }
    }
  }

  /// Corrige la dérive entre la vidéo et l'audio.
  ///
  /// Les deux horloges sont indépendantes : celle du webview pour la vidéo,
  /// celle de la carte son pour Web Audio. Un écart s'installe lentement ;
  /// au-delà du seuil audible, on relance les sources sur la position vidéo.
  tick() {
    if (!this.playing || !this.ready) return 0;
    const audioPosition = this.startOffset + (this.context.currentTime - this.startedAt);
    const drift = audioPosition - this.video.currentTime;
    if (Math.abs(drift) > RESYNC_THRESHOLD) this.start();
    return drift;
  }

  unload() {
    this.stopSources();
    this.tracks.clear();
    this.playing = false;
  }
}
