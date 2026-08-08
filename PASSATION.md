# Reprendre SmartClip Studio

Document court, à lire avant de replonger. Le détail technique est dans
[README.md](README.md) — notamment sa section « Contraintes découvertes », qui
condense les défauts les plus coûteux du projet et ne doit pas être
redécouverte.

**État au 08/08/2026 : application complète et fonctionnelle, validée à l'usage.**

---

## Lancer

```bash
cargo run --release -p smartclip-app
```

L'exécutable à jour est `target\release\smartclip-studio.exe`. ⚠️ **L'application
installée n'est pas mise à jour par un `cargo build`** — c'est une confusion qui
a déjà fait croire à des changements invisibles. Pour rafraîchir l'installeur :

```bash
cargo tauri build --config crates/smartclip-app/tauri.conf.json
```

---

## Ce qui fonctionne, mesuré

| | |
|---|---|
| Capture continue | 1080p60, sans fuite ni orphelin, validée 40 min en jeu |
| Pistes séparées par application | détection automatique, dynamique en cours de session |
| Sauvegarde | ~1,1 s pour un buffer d'une minute |
| Export | 1,8 s pour 54 s, vidéo jamais réencodée |
| Raccourci | personnalisable, avec état affiché |
| Déclenchement vocal | phrase personnalisable, hors ligne |
| Installeur | 2,4 Mo, sans droits administrateur |

Interface : bibliothèque, éditeur avec vumètres animés, forme d'onde navigable,
équilibrage automatique, écoute en direct, overlay en jeu.

---

## Ce qui reste

**Aucun de ces points n'est bloquant pour l'usage quotidien.**

1. **Signature de code** — 1 à 3 semaines de délai administratif, indépendant du
   travail fourni. C'est le chemin critique de toute diffusion : sans elle,
   SmartScreen bloque chaque installation. À engager tôt.
2. **Blocage sous charge** — survenu deux fois avec un jeu lourd, jamais depuis
   les correctifs. Sans parade active : la régulation qui existait a été retirée
   car elle causait un défaut pire. Si le blocage revient, la piste sérieuse est
   de déporter l'encodage dans un thread à file bornée, **sans jamais toucher au
   rythme de la boucle** — c'est ce qui avait tout cassé.
3. **Plein écran exclusif** et **sortie de veille** — non testés.
4. **NVIDIA / Intel** — tout est validé sur une seule Radeon RX 6650 XT. Les MFT
   diffèrent nettement entre constructeurs, le doublement de débit d'AMD en est
   la preuve.
5. **Marqueur rétroactif** — idée retenue, non commencée : poser un repère
   pendant la partie sans rien enregistrer, puis extraire les moments marqués en
   fin de session. Le buffer contient déjà tout.

---

## Comment diagnostiquer

Les cinq bugs les plus graves du projet ont tous été trouvés en usage réel, et
**aucun par raisonnement sur le code**. À chaque fois, une mesure a tranché ce
que plusieurs hypothèses plausibles n'avaient pas résolu.

Deux outils à utiliser en premier, avant toute théorie :

```bash
# Régularité des horodatages vidéo. Une lecture saccadée s'y voit
# immédiatement : intervalle moyen, pires écarts, taux d'irrégularité.
cargo run --release --bin smartclip -- probe "chemin\clip.mp4"

# Extrait chaque piste en WAV. Sert à vérifier qu'une piste contient du
# signal, et à compter les discontinuités qui trahissent un grésillement.
cargo run --release --bin smartclip -- tracks "chemin\clip.mp4"
```

Troisième réflexe, moins évident : **regarder le dossier de travail**
(`%TEMP%\smartclip`). Des fichiers qui s'y accumulent expliquent à eux seuls ce
qui ressemble à une fuite mémoire — le cache d'écriture de Windows les impute au
processus.

Et une campagne d'endurance, qui relève mémoire, erreurs et redémarrages :

```bash
cargo run --release --bin smartclip -- --buffer 60 --duration 1800
```

---

## Deux erreurs à ne pas refaire

**Dater sur une horloge externe ce qui doit simplement se suivre.** Cette erreur
a été commise trois fois : horodatage vidéo au QPC dans les segments, position
audio relative à l'origine du segment, placement des paquets décodés à
l'extraction. À chaque fois, un compteur séquentiel était la bonne réponse.

**Écrire un diagnostic là où l'utilisateur ne peut pas le voir.** L'échec du
raccourci, celui de l'écoute en direct et celui du démarrage de Media Foundation
étaient tous journalisés dans une console absente en release. Trois
fonctionnalités mortes en silence. Tout échec doit remonter à l'interface.

---

## Environnement

Rust + Tauri 2, Windows 11 uniquement. Aucune dépendance Node : la vue est du
HTML, du CSS et du JS servis tels quels depuis `ui/`. Vingt commits, historique
propre.
