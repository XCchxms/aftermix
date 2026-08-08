# Reprendre SmartClip Studio

**À lire en entier avant de toucher au code.** Ce document est autosuffisant :
il contient l'état, les pièges, les outils de diagnostic et les décisions à ne
pas défaire. Le détail technique complet est dans [README.md](README.md), les
règles visuelles dans [DESIGN.md](DESIGN.md).

État au **08/08/2026** : application complète, validée à l'usage par
l'utilisateur. 24 commits, build vert, 24 tests.

---

## 1. Le produit en une phrase

Un buffer vidéo permanent qui enregistre **chaque application dans une piste
audio séparée**, pour rééquilibrer le son *après coup* — le jeu trop fort, le
micro trop faible, Discord qui couvre les voix.

C'est cette séparation qui fait tout : ni ShadowPlay, ni Medal, ni OBS ne
proposent de remixer un clip déjà enregistré.

---

## 2. Lancer et construire

```bash
# Développement — l'exécutable est target\release\smartclip-studio.exe
cargo run --release -p smartclip-app

# Installeur NSIS, dans target\release\bundle\nsis\
cargo tauri build --config crates/smartclip-app/tauri.conf.json
```

> ⚠️ **`cargo build` ne met PAS à jour l'application installée.** Cette
> confusion a déjà fait croire, deux sessions de suite, que des changements
> n'avaient aucun effet. Si tu testes l'application installée, reconstruis
> l'installeur.

Outillage requis : Rust, et `cargo install tauri-cli --version "^2"` pour
l'installeur. **Aucune dépendance Node** : la vue est du HTML/CSS/JS servi tel
quel depuis `ui/`.

---

## 3. Architecture

| Crate / dossier | Rôle |
|---|---|
| `smartclip-core` | Horloge QPC partagée |
| `smartclip-engine` | Tout le multimédia : capture, audio, segments, export, bibliothèque |
| `smartclip-app` | Interface Tauri — n'expose le moteur, ne contient aucune logique métier |
| `ui/` | Vue : `index.html`, `styles.css`, `app.js`, `preview.js`, `overlay.html` |
| `crates/spikes/` | Les quatre prototypes de la Phase 0, **inchangés depuis le début** |

**Les spikes sont la référence de bisection.** Quand le moteur se comporte mal,
comparer avec `spike1_capture` (capture seule) et `spike4_ring` (segments +
recollage) isole la couche fautive en deux commandes. C'est ce qui a résolu le
freeze vidéo après trois hypothèses fausses.

### Le chemin d'un clip

1. **Capture** — Windows Graphics Capture livre des textures GPU ; un
   `IMFSinkWriter` par segment les encode en H.264 sans jamais repasser par la
   mémoire centrale.
2. **Audio** — un client WASAPI de loopback par processus, plus le micro. Les
   applications sont découvertes automatiquement et réévaluées toutes les 3 s.
3. **Segments** — des MP4 autonomes de 8 s, dans un anneau borné **en durée et
   en octets**.
4. **Sauvegarde** — finalisation du segment courant, puis recollage
   *passthrough* : les échantillons compressés sont recopiés sans réencodage.
5. **Export** — l'audio est décodé, mixé selon les gains, réencodé ; la vidéo
   est recopiée telle quelle.

---

## 4. Performances mesurées

| | |
|---|---|
| CPU | ~6 % d'un cœur |
| Mémoire | ~200 Mo, stable sur 40 min |
| Disque | 5,3 Mo/s — soit **~1,6 Go pour 5 min de buffer** |
| Sauvegarde | ~1,1 s pour un buffer d'une minute |
| Export | 1,8 s pour un clip de 54 s à 6 pistes |
| Installeur | 2,4 Mo |

⚠️ Ces chiffres viennent d'une seule machine (Radeon RX 6650 XT). **L'impact sur
les FPS en jeu n'a jamais été mesuré.**

---

## 5. Ce qui reste

Aucun de ces points n'empêche l'usage quotidien.

1. **Signature de code** — 1 à 3 semaines de délai administratif, indépendant du
   travail fourni. **C'est le chemin critique de toute diffusion** : sans elle,
   SmartScreen bloque chaque installation et les antivirus réagissent. À engager
   avant tout le reste si une bêta est envisagée.
2. **Blocage sous charge** — survenu deux fois avec un jeu lourd, jamais depuis
   les correctifs. Sans parade active : celle qui existait a été retirée car
   elle causait un défaut pire. Si le blocage revient, déporter l'encodage dans
   un thread à file bornée — **sans jamais toucher au rythme de la boucle**.
4. **Plein écran exclusif**, **sortie de veille**, **NVIDIA / Intel** — non
   testés. Les MFT diffèrent nettement entre constructeurs.
5. **Vignettes des anciens clips** — elles ne sont créées qu'à la sauvegarde.
   Les clips antérieurs gardent une couverture unie tant qu'un balayage
   rétroactif n'a pas été ajouté : appeler `export::extract_thumbnail` sur tout
   clip dont le `.png` manque, au premier chargement de la bibliothèque.
6. **Marqueur rétroactif** — idée retenue, non commencée. Poser un repère
   pendant la partie sans rien enregistrer, puis extraire les moments marqués en
   fin de session. Le buffer contient déjà tout.

---

## 6. Diagnostiquer

Les cinq bugs les plus graves du projet ont tous été trouvés en usage réel, et
**aucun par raisonnement sur le code**. À chaque fois une mesure a tranché ce
que plusieurs hypothèses plausibles n'avaient pas résolu. Commencer par mesurer.

```bash
# Régularité des horodatages vidéo. Une lecture saccadée s'y voit
# immédiatement. Attendu : ~16,7 ms de moyenne, 0 % d'irréguliers.
cargo run --release --bin smartclip -- probe "chemin\clip.mp4"

# Extrait chaque piste en WAV. Vérifie qu'une piste contient du signal, et
# permet de compter les discontinuités qui trahissent un grésillement.
cargo run --release --bin smartclip -- tracks "chemin\clip.mp4"

# Campagne d'endurance : mémoire, erreurs, redémarrages, sauvegardes.
cargo run --release --bin smartclip -- --buffer 60 --duration 1800
```

**Troisième réflexe, le moins évident** : regarder `%TEMP%\smartclip`. Des
fichiers qui s'y accumulent expliquent à eux seuls ce qui ressemble à une fuite
mémoire — le cache d'écriture de Windows les impute au processus. Une campagne a
ainsi montré 4,6 Go de « fuite » qui n'étaient que 935 Mo de segments orphelins.

Pour l'interface, la console de développement se ferme en release. **Tout échec
doit donc remonter à l'écran** (voir §7).

---

## 7. Les deux erreurs à ne jamais refaire

### Dater sur une horloge externe ce qui doit simplement se suivre

Commise **trois fois**, à trois endroits différents :

- horodatage vidéo au QPC dans les segments → trous jusqu'à **24 secondes**,
  image figée pendant que le son continue ;
- position audio relative à l'origine du segment → paquets écrasés au même
  instant, **grésillement à chaque frontière de segment** ;
- placement des paquets décodés par leur timestamp à l'extraction →
  chevauchements additionnés, **craquement toutes les 21 ms**.

À chaque fois, **un compteur séquentiel était la bonne réponse** : la vidéo en
cadence constante, l'audio par nombre d'échantillons écrits.

### Écrire un diagnostic là où l'utilisateur ne peut pas le voir

Trois fonctionnalités sont mortes en silence parce que leur échec était
journalisé dans une console absente en release : le raccourci global (déjà pris
par un autre logiciel), l'écoute en direct (Media Foundation non démarré) et le
démarrage du buffer. Dans les trois cas, l'utilisateur ne pouvait que constater
que « ça ne marche pas ».

Aujourd'hui l'interface affiche l'état du raccourci, celui de l'écoute vocale,
celui de l'écoute en direct, les pistes silencieuses, les doublons de signal et
le buffer figé. **Toute nouvelle fonctionnalité doit suivre cette règle.**

---

## 8. Décisions à ne pas défaire

Chacune a coûté une session de diagnostic. Le détail est dans le README, section
« Contraintes découvertes ».

| Décision | Pourquoi |
|---|---|
| Vidéo en **cadence constante** dans les segments | le QPC inscrit chaque hoquet de boucle comme un trou dans le fichier |
| Audio positionné par **compteur d'échantillons** | tout horodatage produit chevauchements ou trous |
| Anneau borné **en octets autant qu'en durée** | le MFT AMD ignore tout plafond de débit et produit jusqu'au double de la consigne |
| **Un canal audio par piste** | une file commune laisse la source la plus bavarde affamer les autres |
| Blob d'activation audio **sur le tas** | le service audio le relit après la fin de l'activation ; sur la pile → corruption du tas |
| Ouverture et finalisation des segments **hors de la boucle** | 678 ms de blocage sinon, toutes les 2 s |
| **Régulation du SinkWriter active** en capture, désactivée à l'export | active elle évite une fuite de 4,6 Go ; à l'export elle multiplie le temps par 355 |
| Audio et vidéo **entrelacés** à l'écriture | le muxeur bloque si un flux prend trop d'avance |
| Media Foundation démarré **au lancement de l'application** | sinon `MF_E_SHUTDOWN` dès qu'on ouvre un clip sans avoir démarré le buffer |
| **Une seule instance** | deux applications, ce sont deux buffers capturant le même écran |

---

## 9. Environnement

Rust + Tauri 2, **Windows 11 uniquement** — ce choix supprime tout code de
compatibilité et garantit la disponibilité du loopback par processus.

Windows Graphics Capture plutôt qu'un hook Direct3D : **aucune injection de DLL
dans le jeu**, donc aucun risque anti-cheat. C'est une contrainte de conception,
pas une commodité — y renoncer changerait la nature du produit.
