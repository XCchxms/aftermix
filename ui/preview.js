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

/// Niveau perçu d'un tampon : moyenne quadratique, échantillonnée.
///
/// La crête ne dit rien du volume ressenti — un seul claquement la fait bondir
/// alors que la piste reste discrète. C'est la moyenne quadratique qui
/// correspond à ce qu'on entend, donc à ce qu'il faut équilibrer.
function loudnessOf(buffer) {
  const data = buffer.getChannelData(0);
  let sum = 0;
  let count = 0;
  for (let i = 0; i < data.length; i += 256) {
    sum += data[i] * data[i];
    count++;
  }
  return count > 0 ? Math.sqrt(sum / count) : 0;
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

  /// Enveloppe sonore du clip, en `resolution` points entre 0 et 1.
  ///
  /// Combine toutes les pistes audibles : c'est l'intensité de la scène qu'on
  /// cherche à voir, pas celle d'une source en particulier. Les pics
  /// correspondent aux moments forts — un tir, un cri, une explosion — et
  /// permettent de retrouver l'action sans la chercher à l'aveugle.
  waveform(resolution = 900) {
    const audible = [...this.tracks.values()].filter((t) => t.peak >= SILENCE_PEAK);
    if (audible.length === 0) return null;

    const envelope = new Float32Array(resolution);
    for (const track of audible) {
      const data = track.buffer.getChannelData(0);
      const perPoint = Math.max(1, Math.floor(data.length / resolution));
      for (let point = 0; point < resolution; point++) {
        let peak = 0;
        const start = point * perPoint;
        // Un pas d'échantillonnage à l'intérieur du segment suffit : on cherche
        // la silhouette, pas la précision.
        for (let i = start; i < start + perPoint; i += 8) {
          const value = Math.abs(data[i] ?? 0);
          if (value > peak) peak = value;
        }
        // Les pistes se combinent par leur maximum : additionner ferait
        // saturer l'affichage dès que deux sources parlent ensemble.
        if (peak > envelope[point]) envelope[point] = peak;
      }
    }

    // Normalisation sur le pic réel : un clip discret doit remplir la vue
    // autant qu'un clip fort, sinon il paraît vide.
    const loudest = envelope.reduce((max, v) => Math.max(max, v), 0);
    if (loudest > 0) {
      for (let i = 0; i < envelope.length; i++) envelope[i] /= loudest;
    }
    return envelope;
  }

  /// Niveau courant de chaque piste, entre 0 et 1.
  ///
  /// Destiné à l'affichage : la valeur est déjà lissée par l'analyseur, et une
  /// racine carrée l'étale vers le haut pour que les niveaux faibles restent
  /// visibles — un vumètre linéaire paraît éteint la plupart du temps.
  levels() {
    const levels = new Map();
    if (!this.playing) return levels;

    for (const [index, track] of this.tracks) {
      if (!track.source) {
        levels.set(index, 0);
        continue;
      }
      track.analyser.getByteFrequencyData(track.bins);
      let sum = 0;
      for (const value of track.bins) sum += value;
      const average = sum / track.bins.length / 255;
      levels.set(index, Math.min(1, Math.sqrt(average) * 1.4));
    }
    return levels;
  }

  /// Propose un équilibre entre les pistes audibles.
  ///
  /// Le principe : amener chaque source au même niveau perçu, puis appliquer
  /// une pondération par rôle. Un micro doit passer *au-dessus* du jeu — c'est
  /// la voix qui porte le moment, le jeu n'est qu'un décor sonore. Discord se
  /// place entre les deux : audible sans couvrir.
  ///
  /// Ce n'est qu'une proposition, jamais un réglage imposé : l'utilisateur
  /// garde la main, et c'est bien pour ça qu'il utilise Aftermix.
  suggestGains(labels) {
    const audible = [...this.tracks.entries()].filter(
      ([, track]) => track.peak >= SILENCE_PEAK && track.loudness > 0,
    );
    if (audible.length === 0) return new Map();

    // Référence : la piste la plus forte reste à son niveau, les autres s'y
    // ajustent. Remonter tout le monde saturerait le mixage.
    const reference = Math.max(...audible.map(([, t]) => t.loudness));

    const weightOf = (label = "") => {
      const name = label.toLowerCase();
      if (name.includes("micro")) return 1.25;
      if (name.includes("discord")) return 0.85;
      if (name.includes("musique")) return 0.5;
      return 0.7; // jeu, navigateur, inconnu : le décor
    };

    const gains = new Map();
    for (const [index, track] of audible) {
      const balanced = (reference / track.loudness) * weightOf(labels.get(index));
      // Bornes larges mais fermes : au-delà, on amplifie surtout le bruit de
      // fond, en deçà la piste devient inaudible et autant la couper.
      gains.set(index, Math.min(2, Math.max(0.15, balanced)));
    }
    return gains;
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

      // Un analyseur par piste, branché **après** le gain : le vumètre montre
      // ce qu'on entend réellement, pas ce que contient le fichier. Baisser un
      // fader fait retomber sa barre, ce qui rend le réglage lisible à l'œil
      // autant qu'à l'oreille.
      const analyser = this.context.createAnalyser();
      analyser.fftSize = 256;
      analyser.smoothingTimeConstant = 0.7;
      gainNode.connect(analyser);
      analyser.connect(this.context.destination);
      this.tracks.set(index, {
        buffer,
        gainNode,
        analyser,
        bins: new Uint8Array(analyser.frequencyBinCount),
        source: null,
        peak: peakOf(buffer),
        loudness: loudnessOf(buffer),
      });
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
