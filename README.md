# SmartClip Studio

Capture de clips de jeu dont **les pistes audio restent modifiables après
l'enregistrement**. Le jeu trop fort, le micro trop faible, Discord qui couvre
les voix : on corrige après coup, en quelques secondes.

> **État au 30/07/2026 — Phase 0 terminée, moteur V1 fonctionnel en CLI.**
> Les quatre spikes sont validés et fusionnés dans `smartclip-engine`.
> Il manque l'interface, l'éditeur audio et l'export.
> Il n'y a pas encore d'application : ni interface, ni projet Tauri. Le dépôt ne
> contient que le cœur de l'horloge et deux prototypes headless.

---

## Ce qui existe

```
Cargo.toml                                  workspace
crates/smartclip-core/src/clock.rs          horloge maître QPC (+ tests)
crates/smartclip-engine/src/lib.rs          configuration, timeline commune
crates/smartclip-engine/src/video.rs        capture WGC + D3D11
crates/smartclip-engine/src/audio.rs        découverte et capture par processus
crates/smartclip-engine/src/segment.rs      segments MP4 + anneau borné
crates/smartclip-engine/src/concat.rs       recollage sans réencodage
crates/smartclip-engine/src/export.rs       mixage des pistes et export final
crates/smartclip-engine/src/library.rs      bibliothèque et métadonnées (+ tests)
crates/smartclip-app/src/main.rs            interface Tauri (commandes)
ui/                                         vue : HTML, CSS, JS sans bundler
crates/smartclip-engine/src/recorder.rs     orchestration (API publique)
crates/smartclip-engine/src/bin/smartclip.rs  CLI + raccourci global
crates/spikes/src/bin/spike1_capture.rs     Spike 1 — capture vidéo
crates/spikes/src/bin/spike2_audio.rs       Spike 2 — audio par processus
crates/spikes/src/bin/spike3_sync.rs        Spike 3 — muxage multi-pistes + synchro
crates/spikes/src/bin/spike4_ring.rs        Spike 4 — anneau de segments + sauvegarde
```

## Ce qui n'existe pas

Interface, projet Tauri, bibliothèque de clips, prévisualisation temps réel du
mixage.

**Séparation des sources vérifiée sur un vrai clip de jeu.** Capture pendant une
partie de Fortnite, pistes mesurées séparément :

| Piste | Crête | |
|---|---|---|
| Jeu | 0,703 | signal franc |
| Micro | 0,546 | signal franc |
| Discord | 0,032 | ⚠️ **aucun vocal en cours** — application ouverte, personne ne parlait |

Jeu et micro sont bien captés sur deux pistes distinctes, sans configuration :
c'est la mécanique de séparation qui est validée.

⚠️ **Le rééquilibrage entre voix concurrentes reste non démontré.** Il faudrait
un clip avec un vocal Discord actif *pendant* le jeu — le scénario « Discord
couvre les voix » du cahier des charges. Une piste presque muette ne prouve rien
sur ce point.

**L'audio et la vidéo doivent être entrelacés à l'export.** Écrire toute la
piste audio avant la première image demande au muxeur de la retenir en attendant
que la vidéo la rattrape ; passé une certaine avance, sa régulation bloque
`WriteSample` et l'export ne se termine jamais. Un clip de 14 s passait, un clip
de 60 s figeait le processus plus de six minutes. La vidéo avance désormais
image par image, les blocs audio étant intercalés dès que leur horodatage est
rattrapé — aucun flux ne prend plus d'un bloc d'avance.

**Un `IMFSinkWriter` créé sans attributs écrit 355 fois plus lentement.**
L'écriture d'un clip de 54 s prenait 201 s ; avec
`MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS` et `MF_SINK_WRITER_DISABLE_THROTTLING`,
elle prend **566 ms**. Le writer d'export avait été créé avec `None` comme
attributs, contrairement à celui des segments — un oubli invisible à la lecture
du code, et introuvable sans profilage.

→ C'est le seul endroit où désactiver la régulation est légitime : le contenu
est borné, entièrement en mémoire, et l'entrelacement garantit qu'aucun flux ne
prend d'avance. En capture continue, la même option provoquait une fuite de
4,6 Go.

**Export mesuré : 1,8 s pour un clip de 54 s à 6 pistes** (823 ms de mixage,
566 ms d'écriture), fichier valide et relu par Windows.

## Lancer l'application

```bash
cargo run --release -p smartclip-app
```

Bibliothèque en grille, éditeur avec lecteur, un fader par piste, **écoute en
direct** et export.

**Ctrl+Shift+X** sauvegarde un clip, y compris fenêtre fermée : l'application
vit dans la barre système et le buffer continue de tourner pendant qu'on joue.
On quitte par le menu de la barre système, jamais par la croix.

Le raccourci est **personnalisable** dans les réglages, et son état y est
affiché : `✓ actif` ou la raison exacte du refus. C'est indispensable — une
combinaison déjà prise par un autre logiciel est refusée par Windows, et sans
cet indicateur la fonction principale du produit reste inutilisable sans que
rien ne l'explique.

Chaque sauvegarde déclenche **un overlay** en haut à droite de l'écran, visible
pendant une partie : fenêtre sans décor, toujours au premier plan, jamais
focusable et transparente aux clics. Les notifications de Windows sont
supprimées ou reléguées derrière dès qu'un jeu occupe l'écran — le retour le
plus important du produit passait donc inaperçu au moment précis où il compte.
La notification système est conservée en plus, pour retrouver après coup ce
qu'on a manqué.

⚠️ En plein écran **exclusif**, aucune fenêtre ne peut se superposer au jeu.
C'est une contrainte de Windows : y parvenir exigerait d'injecter un overlay
dans le moteur de rendu du jeu, précisément ce que le projet s'interdit pour ne
pas éveiller les anti-cheat. Les jeux en plein écran fenêtré, largement
majoritaires, ne sont pas concernés.

Les réglages (⚙) proposent 30 s, 1 min, 3 min ou 5 min de buffer et le dossier
des clips ; ils sont conservés dans `%APPDATA%\SmartClip\settings.json`. Changer la durée pendant que le buffer tourne le redémarre : elle est
fixée à l'ouverture de la capture, et sans redémarrage le réglage ne prendrait
effet qu'à la session suivante.

L'écoute en direct est le cœur de l'expérience : bouger un fader pendant la
lecture s'entend immédiatement, sans exporter. Le webview ne sachant lire qu'une
piste d'un MP4 multi-pistes, le moteur les extrait en WAV (6 pistes d'un clip de
10 s en 0,42 s) ; la vue les charge en `AudioBuffer`, la vidéo joue muette comme
horloge maître et chaque piste passe par son propre `GainNode`.
Interface sombre, sans dépendance Node : la vue est du HTML, du CSS et du JS
servis tels quels par Tauri, ce qui supprime toute chaîne de build front.

⚠️ **L'interface n'a pas encore été éprouvée à l'usage.** Elle compile, se lance
et affiche sa fenêtre ; le parcours complet — démarrer le buffer, sauvegarder,
régler les faders, exporter — demande une vérification à la main.

## Produire l'installeur

```bash
cargo tauri build --config crates/smartclip-app/tauri.conf.json
```

Produit `SmartClip Studio_0.1.0_x64-setup.exe` (**2,3 Mo**) dans
`target/release/bundle/nsis/`. Installation par utilisateur, sans droits
administrateur. Le binaire lui-même fait 10,2 Mo — l'interface s'appuie sur le
WebView2 du système, présent d'origine sur Windows 11.

À comparer aux ~150 Mo d'une application Electron équivalente : c'est ce que
valait le choix de Rust + Tauri pour un logiciel censé tourner en permanence.

⚠️ **L'installeur n'est pas signé.** SmartScreen affichera un avertissement et
certains antivirus pourront réagir : une application qui capture l'écran et
l'audio de plusieurs processus présente exactement le profil qu'ils surveillent.
Un certificat de signature de code est indispensable avant toute diffusion, même
en bêta fermée.

## Lancer le moteur en ligne de commande

```bash
cargo run --release --bin smartclip -- --buffer 60
```

Campagne d'endurance — laisse tourner, relève mémoire, erreurs et
redémarrages, et sauvegarde toutes les 2 minutes :

```bash
cargo run --release --bin smartclip -- --buffer 60 --duration 1800
```

Le buffer tourne en continu ; **Ctrl+Shift+X** fige les dernières secondes dans
`%USERPROFILE%\Videos\SmartClip`, **Ctrl+C** quitte. `--auto-save <s>` sauvegarde
une fois puis rend la main, sans dépendre d'une frappe.

Mesuré sur AMD Radeon RX 6650 XT, 1080p60, buffer de 15 s :

```
pistes audio détectées : Jeu, Discord, Micro
✅ clip_1785426685.mp4 — 14.1s, 11 Mo, 3 pistes,
   sauvegardé en 294 ms (finalisation 8 ms + recollage 285 ms)
```

Les pistes sont découvertes et nommées sans aucune configuration — c'est la
promesse centrale du produit, et elle tient.

## Rééquilibrer un clip

```bash
cargo run --release --bin smartclip -- mix clip.mp4 --gains "1.0,0.4,1.0"
```

Un gain par piste, dans l'ordre du fichier ; `0` coupe. Sans `--gains`, toutes
les pistes passent à l'identique. La vidéo est **recopiée sans réencodage**,
seul l'audio est décodé, mixé et réencodé : un clip de 14 s s'exporte en 0,69 s.

Mesuré sur un clip de 54 s à 6 pistes issu d'une vraie partie : **1,8 s** au
total, vidéo recopiée sans réencodage.

Pour lister la bibliothèque :

```bash
cargo run --release --bin smartclip -- list
```

Si le cumul des gains dépasse la pleine échelle, un limiteur s'applique et la
crête est signalée — à l'interface d'inviter à baisser les faders plutôt que de
livrer une distorsion découverte à la lecture.

---

## Lancer les prototypes

```bash
cargo test -p smartclip-core
```

```bash
cargo run --release --bin spike1_capture -- --minutes 1 --rc 1
```

```bash
cargo run --release --bin spike2_audio -- --seconds 15
```

```bash
cargo run --release --bin spike3_sync -- --minutes 5
```

```bash
cargo run --release --bin spike4_ring -- --minutes 1 --buffer 30 --max-mb 1024
```

`spike2` n'a d'intérêt que si quelque chose émet du son (jeu, Spotify, une vidéo
dans le navigateur) — sinon il ne découvre aucune source.

Options utiles de `spike1` : `--fps`, `--bitrate`, `--rc` (0=CBR,
1=PeakConstrainedVBR, 2=UnconstrainedVBR, 4=LowDelayVBR), `--out`.
Options de `spike2` : `--discover-only`, `--activate-only`, `--no-mic`,
`--no-loopback`, `--outdir` (elles servaient à bisecter, elles restent utiles).

---

## Décisions figées

| Sujet | Choix |
|---|---|
| Langage / UI | **Rust + Tauri 2 + React/TS** — binaire ~15 Mo pour une app qui tourne en permanence, et une UI web pour la finition visuelle visée |
| Plateforme | **Windows 11 uniquement** — supprime tout code de compatibilité, garantit le loopback par processus, permet la capture sans bordure jaune |
| Capture écran | `Windows.Graphics.Capture` — pas d'injection DLL, donc aucun risque anti-cheat |
| Encodage | Media Foundation SinkWriter + `MF_SINK_WRITER_D3D_MANAGER` (textures GPU, zéro readback CPU) |
| Audio | `ActivateAudioInterfaceAsync` sur `VAD\Process_Loopback`, un client par PID, découverte via `IAudioSessionManager2` |
| Horloge | QPC unique pour toutes les sources — jamais `Instant::now` dans les modules de capture |
| Stockage interne | **MP4 multi-pistes** (1 vidéo H.264 + N AAC), muxé par le SinkWriter — validé au Spike 3, ce qui retire MKV et ffmpeg du chemin du buffer |
| Horodatage vidéo | **QPC**, jamais l'indice de frame — voir les contraintes ci-dessous |
| Export | MP4, vidéo **copiée sans réencodage** + AAC mixé → export d'un clip de 30 s en ~1 s |
| ffmpeg | binaire *sidecar* embarqué, pas de dépendance système. **Plus nécessaire pour le buffer** ; reste à trancher s'il l'est encore pour l'export (Media Foundation pourrait suffire : SourceReader → mixage → SinkWriter) |

---

## Résultats mesurés

### Spike 1 — capture vidéo continue ✅

Sur AMD Radeon RX 6650 XT, 1920×1080@60 :

- 600,0 s d'affilée, 36 000 frames, **60,0 fps tenus**
- RSS 73 → 77 Mo : **aucune fuite**
- MFT matériel confirmé (`MFT_ENUM_HARDWARE_URL_Attribute` présent)
- Dérive timeline CFR vs QPC : **−11,2 ms sur 600 s** (~19 ppm)

### Spike 2 — audio séparé par processus ✅

- Découverte et classification automatiques de 3 applications + le micro
- **4/4 pistes**, chacune à 99,9 % de couverture temporelle
- Étalement des premiers paquets entre pistes : **2,4 ms**
- WAV relus sans erreur par le décodeur de Windows

### Spike 3 — muxage multi-pistes et synchro ✅ (risque critique R1)

Run de 5 minutes, 1920×1080@60 + 4 pistes audio, 18 000 frames, 120 008 paquets
audio muxés dans un seul MP4 :

| Piste | Échantillons | QPC | Dérive |
|---|---|---|---|
| loopback ×3 | 300,020 s | 300,020 s | **+0,0 ms** |
| micro | 300,020 s | 300,016 s | **+3,6 ms** |

- Écart entre pistes à 5 min : **3,6 ms** (inaudible)
- Fichier vérifié : **5 boîtes `trak`** (1 vidéo + 4 audio), lu par Windows avec
  vidéo et audio présents

Les horloges des périphériques audio et le QPC concordent à la milliseconde sur
5 minutes. **La dérive audio n'est pas un problème** — contrairement à ce que
l'analyse initiale redoutait.

### Spike 4 — anneau de segments et sauvegarde ✅ (risque R3)

Segments de 2 s, vidéo + 4 pistes audio, anneau borné en durée et en octets :

| Mesure | Résultat |
|---|---|
| Rotation de segment | **0,3 ms** en moyenne, 0,9 ms au pire |
| Finalisation au raccourci | **12 ms** |
| Concaténation sans réencodage | **409 ms** pour 20 s / 103 Mo |
| **Sauvegarde totale** | **421 ms** — critère < 1 s tenu |
| Purge par budget d'octets | 17 purges déclenchées, anneau tenu sous le plafond |

Clip vérifié : **5 boîtes `trak`**, lu par Windows avec vidéo et audio. La
concaténation *passthrough* (SourceReader sans décodeur → SinkWriter) recopie
les échantillons H.264 et AAC sans les toucher — 20 s de vidéo traitées en
409 ms sont hors de portée d'un réencodage.

---

## Contraintes découvertes (à ne pas réapprendre)

**Le blob d'activation audio doit vivre sur le tas.** Le service audio relit
`AUDIOCLIENT_ACTIVATION_PARAMS` *après* que `ActivateCompleted` a signalé la fin
de l'activation. Sur la pile, le cadre est déjà recyclé →
`STATUS_HEAP_CORRUPTION` (0xC0000374). Symptôme fourbe : les pistes
s'enregistrent correctement, le crash ne tombe qu'à la sortie du processus.
Corrigé par `Box::leak` (12 octets par piste, une fois par session).

**Le MFT AMD ne respecte aucun plafond de débit.** En CBR il produit exactement
le double de la consigne ; en `PeakConstrainedVBR` à 20 Mbps il est sorti à
27,5 Mbps sur le run de 10 min. `MF_MT_AVG_BITRATE`, `AVEncCommonMeanBitRate` et
`AVEncCommonMaxBitRate` se relisent tous correctement — et sont ignorés.
→ **L'anneau de segments doit être borné à la fois en durée et en octets**, avec
purge sur le premier seuil atteint. Le budget disque se mesure, il ne se déduit
jamais de la configuration.

**Une application silencieuse produit quand même un flux plein.** Le loopback par
processus délivre un flux continu de zéros à cadence pleine. Conséquence
heureuse : toutes les pistes ont rigoureusement la même longueur, le muxeur n'a
aucun trou à combler. Conséquence coûteuse : **1,92 Mo/s en permanence** pour 5
pistes en float32, soit 576 Mo sur un buffer de 5 minutes. Il faut compresser
l'audio dans les segments — le FLAC réduit une piste silencieuse à presque rien,
sans perte donc sans compromis sur l'édition.

**`CBR` et `LowDelayVBR` sont à écarter sur AMD.** Utiliser `--rc 1`.

**Dans les segments, la vidéo se date en cadence constante — pas au QPC.**
Le QPC paraît plus juste, c'est l'horloge de l'audio, mais il inscrit dans le
fichier chaque hoquet de la boucle : un retard de deux secondes devient un trou
de deux secondes, image figée et son qui continue. Mesuré sur un clip réel :
**83 ms d'intervalle moyen, des trous jusqu'à 24 secondes, 12 images/s**.

La bisection a été faite en comparant avec du code inchangé depuis le début —
`spike1_capture` (capture seule) et `spike4_ring` (segments + recollage) donnent
tous deux **16,7 ms et 60 images/s, zéro irrégularité**. Deux différences les
séparaient du moteur, toutes deux introduites après coup :

1. Le compteur d'images n'avançait que lorsqu'une image arrivait, au lieu
   d'avancer à chaque tour d'horloge : les horodatages se désolidarisaient du
   temps qui passe dès qu'une image manquait.
2. La régulation adaptative sautait des images sur écriture lente.

→ La commande `smartclip probe <clip.mp4>` mesure la régularité des
horodatages. C'est elle qui a permis de trancher : **mesurer plutôt que
raisonner sur du code qu'on vient d'écrire**.

Note historique — la remarque ci-dessous portait sur le Spike 3, où la vidéo
était écrite d'un seul tenant, sans segments. Elle ne s'applique pas au moteur :

**~~La vidéo doit être datée au QPC, jamais à l'indice de frame.~~** Le premier run
de 5 min du Spike 3 datait les frames en `index × durée_nominale` : l'écart avec
le QPC atteignait **−33 ms** et fluctuait au rythme de la gigue du `sleep`
(−16,9 / −26,7 / −36,5 / −26,4 / −33,0 ms). Comme l'audio est daté au QPC, cet
écart était une désynchro A/V réelle dans le fichier — il passait sous les 40 ms,
mais de justesse, et une machine chargée les dépasserait. Daté au QPC, l'écart
devient **nul par construction** : ce n'est plus une valeur à mesurer mais une
propriété de la conception. Le mode fautif reste disponible via
`--video-pts cfr` pour pouvoir le remesurer.

→ Contre-idée à écarter : « l'anneau de segments a besoin d'une cadence
constante ». Non — il a besoin de **keyframes régulières**, pas de PTS
régulières, et le MP4 accepte parfaitement un débit variable.

**Ouvrir et finaliser un segment doit se faire hors de la boucle de capture.**
Le premier essai du Spike 4 appelait `Finalize()` puis recréait le SinkWriter
dans la boucle : **678 ms de blocage en moyenne, 763 ms au pire**, toutes les
2 secondes, pendant lesquelles l'image restait figée — un tiers du temps de
capture perdu. `Finalize` écrit l'index du MP4 et la création d'un SinkWriter
réinitialise le MFT matériel. Déportées sur deux threads (un qui pré-ouvre le
segment suivant, un qui finalise le précédent), la rotation tombe à **0,3 ms**.
La rotation ne doit être qu'un échange de pointeur.

**Le budget en octets se dimensionne sur le débit réel, pas sur la consigne.**
Mesuré au Spike 4 : **5,3 Mo/s** avec 4 pistes audio, soit ~42 Mbps de vidéo
pour une consigne de 20 Mbps — le doublement AMD, confirmé une troisième fois.
Un buffer de 5 minutes réclame donc **~1,6 Go**. Un plafond d'octets trop serré
ne casse rien mais rogne silencieusement la durée réellement disponible : avec
`--max-mb 40`, l'anneau demandé à 60 s n'en conservait que 5,6.

**Les utilitaires constructeur monopolisent les emplacements de capture.**
AMDNoiseSuppression, AMDRSServ, ROCCAT_Swarm_Monitor et consorts ouvrent une
session audio sans jamais produire de son. Sur un essai réel, trois d'entre eux
occupaient trois places sur quatre et le jeu n'était présent que par chance de
l'ordre d'énumération. D'où la liste `UTILITIES` dans `audio.rs` — nécessairement
incomplète, mais une inconnue qui passe le filtre atterrit simplement dans le
mixeur, ce qui est le bon comportement par défaut.

**Les emplacements de pistes sont fixes, et c'est le recollage qui l'impose.**
Concaténer sans réencodage suppose que tous les segments partagent la même
structure de flux : on ne peut donc pas ajouter une piste quand une application
se lance. Le moteur réserve `track_slots` emplacements au démarrage et y affecte
les applications à la volée, les vacants étant remplis de silence — un flux
déclaré mais jamais alimenté empêche la finalisation du segment. Les changements
ne s'appliquent qu'aux frontières de segment, où ils ne peuvent pas produire de
chevauchement d'échantillons.

**Le moteur se détectait lui-même comme source à enregistrer.** Ouvrir un client
de capture crée une session sur le périphérique de sortie, que le balayage
suivant voyait comme une application à capturer : SmartClip s'attribuait une
piste à son propre nom. D'où l'exclusion du PID courant dans `discover`.

**Le silence doit s'écrire par blocs de 100 ms, pas à la cadence vidéo.** Écrit à
chaque frame, il produisait 60 paquets par seconde et par piste vacante ; le
conteneur se fragmentait au point de faire passer un export de 0,7 s à **17 s**.
Par blocs de 100 ms — ce qu'écrivent les vraies pistes — l'export retombe à
0,36 s.

**Tauri 2 : le protocole `asset` se restreint par la config, pas par une
capacité.** `core:asset-protocol:allow-read` n'existe pas ; le seul point de
contrôle est `app.security.assetProtocol.scope` dans `tauri.conf.json`, et la
feature `protocol-asset` doit être activée sur le crate `tauri`. Une icône
`icons/icon.ico` est par ailleurs obligatoire pour construire sur Windows, même
sans empaquetage.

**Les segments orphelins passent pour une fuite mémoire.** Un segment n'entre
dans l'anneau que s'il a été correctement fermé *et* que son information est
revenue au moteur ; tout ce qui échappe à ce chemin reste sur le disque
indéfiniment. Une campagne de 30 min a laissé **27 fichiers pour un buffer qui
devait en garder 7**, soit 935 Mo — et le cache d'écriture de Windows les impute
au processus, d'où un RSS qui grimpait à 4,6 Go. L'anneau lui-même fonctionnait
(les clips faisaient bien la durée voulue), ce qui a longtemps égaré le
diagnostic. `SegmentRing::sweep` efface désormais, à chaque rotation, tout
fichier antérieur au plus ancien segment retenu.

→ Leçon de méthode : deux hypothèses plausibles sur le code ont été démenties
par la mesure. **Regarder le dossier de travail** a tranché en trente secondes.

**Sous charge GPU réelle, `WriteSample` finit par ne plus rendre la main.** Une
campagne lancée pendant une partie de Fortnite s'est figée après 90 s : 41
threads en attente, plus une ligne de journal pendant deux heures, et aucun
compteur d'erreur pour le signaler. C'est le pire mode de défaillance possible —
l'application paraît saine et n'enregistre rien.

Deux parades conservées :
- **Battement de cœur** : la boucle horodate chaque image ; au-delà de 15 s de
  silence, `Health::stalled()` bascule et l'interface alerte.
- **Attentes bornées** : `recv_timeout` partout où un `recv` nu pouvait figer le
  moteur — attente de segment, réponse à une sauvegarde.

⚠️ Une troisième parade, une **régulation adaptative** qui sautait des images sur
écriture lente, a été **retirée** : elle produisait le défaut qu'elle prétendait
éviter (voir plus bas). Le blocage sous charge n'a donc plus de parade active
autre que la détection. S'il réapparaît, il faudra une autre approche —
probablement déporter l'encodage dans un thread à file bornée, plutôt que
toucher au rythme de la boucle.

Une campagne de 40 min pendant une partie de Fortnite est allée à son terme
(0 erreur, mémoire stable, 31 images sautées sur ~144 000). Mais une reproduction
ultérieure avec Rocket League s'est **figée en moins de 22 secondes**.

🟠 **Après trois correctifs** — canal par piste, régulation étendue aux écritures
audio, segments vides écartés — une campagne de 25 min avec source audio
continue est allée à son terme : 0 erreur, **5 images sautées** (contre 31),
mémoire stable (creux 120 → 125 Mo), 12 sauvegardes à 1 095 ms de moyenne,
0 orphelin. Le blocage n'est pas réapparu.

⚠️ Mais cette campagne n'imposait pas une charge GPU comparable à une vraie
partie, et c'est dans ces conditions que le blocage s'était déclenché. **À
confirmer en jeu avant de le déclarer résolu.**

Le défaut d'origine, pour mémoire :

🔴 ~~**Le blocage sous charge n'est PAS résolu.**~~ La régulation ne mesure que
la durée des écritures *vidéo* ; si `WriteSample` bloque sur une piste audio,
rien ne le voit venir. Les protections font leur travail — détection, abandon du
thread figé, délai d'attente sur la sauvegarde — mais la cause persiste.

✅ **Corrigé — un canal borné par piste.** Reproduction après correction, avec
Rocket League, un lecteur audio et le micro actifs : trois pistes distinctes
portant chacune du signal (0,158 / 0,100 / 0,046), sauvegarde en 394 ms, aucun
paquet écarté. **C'est la première démonstration complète de la promesse du
produit.**

Le défaut d'origine, pour mémoire :

🔴 ~~**Le canal audio partagé affame les pistes.**~~ Les six pistes se partagent un
seul canal borné. Quand le muxeur ralentit, la source la plus bavarde — le
micro, en capture WASAPI classique — occupe la quasi-totalité des emplacements :
**7 001 paquets écartés sur la seule piste 5** lors de la reproduction. Les
pistes de loopback perdent alors presque tout leur contenu, et le clip final ne
contient plus que le micro. C'est exactement le symptôme rapporté par
l'utilisateur — un défaut que la mesure sur clip isolé ne pouvait pas montrer,
puisque les clips analysés provenaient de captures qui n'avaient pas saturé.

→ Correction à faire : **un canal borné par piste**, pour qu'une source bavarde
ne puisse pas affamer les autres.

**Ne jamais désactiver la régulation du SinkWriter.** `MF_SINK_WRITER_DISABLE_THROTTLING`
supprime toute contre-pression : dès que l'encodeur prend du retard, le writer
continue d'accepter des échantillons et les empile, chacun retenant une texture
1080p de 8 Mo. Une campagne de 30 min a vu la mémoire passer de **87 Mo à
4,6 Go** — stable pendant 26 minutes, puis explosion après un incident
d'encodage (`E_UNEXPECTED`, 118 échecs en deux secondes). Avec la régulation,
`WriteSample` peut bloquer brièvement et l'on perd quelques images : sans
commune mesure. L'attribut était hérité du Spike 1, qui ne durait pas assez pour
le révéler.

**Le canal audio doit être vidé même quand la vidéo échoue.** Le drainage était
conditionné à la réussite de l'écriture vidéo : pendant un incident, les paquets
s'entassaient dans un canal non borné. Une panne d'encodeur se doublait ainsi
d'une seconde fuite mémoire.

**Le coût d'une sauvegarde est dominé par le nombre de segments, pas par leur
contenu.** Le recollage ouvre un `IMFSourceReader` par segment et chaque
ouverture initialise un pipeline Media Foundation. Mesuré en campagne : un
buffer de 58 s découpé en segments de 2 s — soit 30 fichiers — se sauvegardait
en **5,6 s**, très loin de la seconde visée. Les essais antérieurs portaient sur
15 à 20 s de buffer et ne pouvaient pas le révéler.

→ Contre-intuitivement, **allonger les segments ne coûte rien** : le segment
courant est finalisé à la demande, donc rien n'est perdu quelle que soit sa
durée. Elle ne joue que sur la granularité de la purge. D'où le passage à 8 s
par défaut, qui ramène la sauvegarde d'une minute de buffer à **1,27 s**.

⚠️ **Le critère « sauvegarde < 1 s » du Spike 4 portait sur 20 s de buffer.**
Mesuré à l'échelle réelle : ~1,3 s pour 1 minute, et par extrapolation ~4 s pour
5 minutes. Tenir la seconde sur un buffer de 5 min exigerait de paralléliser
l'ouverture des lecteurs — non fait.

**Discord, Steam et les navigateurs ouvrent plusieurs processus de même nom qui
rendent chacun du son.** Dédoublonner par PID laissait donc deux faders
identiques dans le mixeur — `Discord (Discord)` deux fois, observé en campagne —
et gaspillait un emplacement. Le loopback étant activé en mode « arbre de
processus », le premier client capte déjà tout l'ensemble : le dédoublonnage
porte donc sur l'**exécutable**. Même raisonnement pour la libération d'un
emplacement : un processus qui redémarre change de PID sans que l'application
ait disparu.

**Une source audio qui meurt doit rendre son emplacement au silence.** Sinon son
flux n'est plus alimenté par personne — ni par elle, ni par le silence, puisque
l'emplacement reste marqué occupé — et la finalisation du segment peut se
bloquer : la panne d'un micro suffirait à figer tout l'enregistrement. Chaque
thread de capture signale sa fin par un drapeau (posé même en cas de panique,
via un garde de `Drop`), et le moteur libère l'emplacement à la rotation
suivante. Le micro, qui n'a pas de PID et n'est donc jamais redécouvert par le
balayage, est relancé à part — avec une temporisation, faute de quoi une machine
sans micro lancerait un thread toutes les deux secondes.

**La veille invalide le périphérique Direct3D.** Une mise en veille, une mise à
jour de pilote ou un plantage GPU rendent textures et encodeur inutilisables ;
rien ne se répare en place. Un poste qui dort chaque nuit rencontre ce cas
quotidiennement. `GetDeviceRemovedReason` est interrogé à chaque rotation de
segment, et le traitement est le même qu'un changement de définition : tout
reconstruire.

→ Les redémarrages automatiques sont **bornés** : plus de cinq en moins d'une
minute et le moteur abandonne en le disant. Une rafale ne se résoudra pas
d'elle-même, et insister ferait tourner une boucle inutile. Chaque tentative est
précédée de deux secondes de pause, sans quoi la reconstruction échoue souvent
et déclenche le tour suivant.

**Un jeu qui passe en plein écran change souvent la définition de l'écran.** La
définition est figée dans le flux vidéo du segment, et les segments doivent tous
partager la même structure pour être recollés : la capture ne peut donc pas
continuer telle quelle. Le moteur détecte le changement à la frontière de
segment et **se relance de lui-même**. Le buffer déjà constitué est perdu — il
est à l'ancienne définition — mais la capture reprend au lieu de mourir
précisément au moment où l'utilisateur commence à jouer. L'interface le signale,
pour qu'on ne croie pas couvrir encore les minutes précédentes.

**Un buffer qui s'arrête en silence est le pire défaut possible.** L'utilisateur
joue des heures en se croyant couvert, puis n'a rien au moment voulu. La boucle
tolère donc les échecs d'écriture passagers — disque momentanément saturé,
encodeur qui hoquette — et ne renonce qu'après 120 images consécutives, soit
deux secondes. L'état (`Recorder::health`) est partagé, l'interface l'interroge
toutes les 3 s et alerte au lieu d'afficher un voyant vert mensonger.

**Le nom des pistes vit dans un sidecar JSON, pas dans le MP4.** Media Foundation
n'offre pas de moyen simple d'écrire puis relire un nom de piste. Sans lui, le
mixeur afficherait « piste 0, piste 1 » là où l'utilisateur attend « Jeu,
Discord » — la fonction principale perdrait son sens. Le MP4 reste lisible et
partageable seul ; un clip dont le sidecar manque reste listable, avec des
libellés de repli.

**La capture disjointe de Rust 2021 traverse les wrappers `unsafe impl Send`.**
Une closure qui écrit `wrapper.0` capture le champ COM nu, non `Send`, et non le
wrapper. Passer par une méthode (`wrapper.get()`) force la capture de la
structure entière.

**Les voix individuelles d'un vocal Discord ne sont pas séparables.** Discord mixe
tous les participants avant de rendre l'audio à Windows. À retirer de toute
promesse produit : on isole *Discord*, pas *un ami*.

**windows-rs 0.62, pièges rencontrés** : `implement` n'est pas une feature (il est
exporté sans condition depuis `windows-core`, qui doit être une **dépendance
directe** car la macro génère du code référençant `windows_core`) ;
`ICodecAPI::SetValue` et `VARIANT` exigent `Win32_System_Ole` ; `CreateEventW`
exige `Win32_Security` ; `WAVE_FORMAT_IEEE_FLOAT` n'est pas exposé.

---

## Suite

### Phase 0 — terminée

- [x] ~~**Spike 3 — muxage QPC vidéo + N pistes, dérive à 5 min. CRITIQUE.**~~
      **Validé.** 3,6 ms entre pistes à 5 min, MP4 à 5 pistes lu par Windows.
      Deux acquis : le muxage multi-pistes par le SinkWriter fonctionne (adieu
      MKV et ffmpeg sur le chemin du buffer), et la vidéo doit être datée au QPC.
- [x] ~~**Spike 4 — anneau de segments disque + concat instantané.**~~
      **Validé.** Sauvegarde en 421 ms, rotation à 0,3 ms, purge par budget
      d'octets vérifiée. La concaténation *passthrough* Media Foundation retire
      ffmpeg du chemin de sauvegarde comme du chemin de buffer.

**Phase 0 terminée : aucun risque bloquant ne subsiste.**

### V1

1. ~~Moteur complet, pilotable en CLI, sans interface~~ — **fait**, y compris la
   détection des applications en cours de session. Reste à durcir : tri par
   activité audio réelle plutôt que par ordre d'énumération, reprise après
   changement de périphérique audio, et masquage des emplacements vacants dans
   l'interface (aujourd'hui exposés sous le libellé `(libre N)`).
2. ~~Export mixé~~ — **fait.** `mix_and_export` applique un gain par piste,
   recopie la vidéo sans réencodage et signale les dépassements.
3. Interface — **faite** : bibliothèque, éditeur, écoute en direct, raccourci
   global, barre système, réglages. Reste à éprouver à l'usage et à polir.
4. Stabilisation : 8 h de buffer continu, cas limites, installeur signé (2 sem.)

Périmètre V1 : 1080p60, moniteur principal, buffer 30 s / 1 / 3 / 5 min,
5 pistes auto-détectées, bibliothèque, éditeur à faders, export MP4, raccourci
global, icône de barre système.

Hors V1 : multi-écran, 4K/120, coupe et rognage, effets audio, partage direct,
cloud, webcam, overlay in-game.

### Prévisualisation dans l'éditeur — implémentée

Vidéo muette comme horloge maître, pistes décodées en `AudioBuffer` (Web Audio),
un `GainNode` par piste, resynchronisation au-delà de 40 ms de dérive. Faire
jouer 5 éléments `<audio>` en parallèle dérive systématiquement — ne pas tenter.

Voir `ui/preview.js`. Deux détails qui conditionnent le fonctionnement : le CSP
doit autoriser `connect-src ... asset:` (le `fetch` d'un asset ne relève pas de
`media-src`), et `$TEMP` doit figurer dans le `scope` du protocole asset,
puisque c'est là que les WAV sont extraits.

---

## Registre des risques

| # | Risque | État |
|---|---|---|
| R1 | Dérive A/V sur clips longs | ✅ **levé** — 3,6 ms entre pistes à 5 min, à condition de dater la vidéo au QPC |
| R2 | Loopback silencieux sur certains jeux | ✅ levé (filet « Autres » validé) |
| R3 | Écriture disque continue mal perçue | ✅ **levé** — anneau doublement borné, sauvegarde en 421 ms |
| R4 | Saturation des sessions encodeur matériel | ⏳ |
| R5 | Plein écran exclusif : écran noir ou raccourci avalé | ⏳ non testé — le changement de définition qui l'accompagne est traité (redémarrage automatique), le reste demande un essai en jeu |
| R6 | Bordure jaune de capture | ✅ `SetIsBorderRequired(false)` accepté |
| R7 | Contenu protégé DRM → cadre noir | comportement Windows, à documenter |
| R8 | Faux positif antivirus | ⚠️ **devenu concret** — l'installeur existe mais n'est pas signé. Prévoir un certificat de signature de code (délai administratif à ne pas découvrir au dernier moment) |

⚠️ Les mesures de débit ont été faites sur un bureau quasi immobile. **Le
dimensionnement définitif de l'anneau exige une mesure sur du vrai gameplay.**
