# Direction visuelle

Les règles suivies dans `ui/styles.css`. À lire avant d'y toucher : chacune
répond à un problème constaté, pas à une préférence.

---

## Le principe

L'application s'ouvre **par-dessus un jeu**, souvent en pleine action. Tout
découle de là : fond sombre obligatoire, information hiérarchisée pour être lue
d'un coup d'œil, aucun élément qui bouge sans raison.

## Couleur

**Des gris neutres, sans dominante colorée.** Une teinte froide appliquée à
toute l'interface — bleutée, violette — produit ce rendu « science-fiction » que
tous les utilitaires de capture se croient obligés d'adopter, et qui date
immédiatement un logiciel. Un neutre franc laisse au contenu, les vignettes, le
soin d'apporter la couleur.

**L'action principale est blanche.** Sur fond sombre, rien ne ressort davantage.
C'est aussi ce qui évite le bouton coloré saturé et le dégradé à deux teintes,
signes distinctifs d'une interface qui essaie trop.

La couleur est **réservée au sens**, jamais décorative. Quatre valeurs, pas une
de plus :

| | |
|---|---|
| **vert** `--live` | le buffer enregistre |
| **ambre** `--warn` | attention : sauvegarde en cours, écoute indisponible, son en double |
| **rouge** `--danger` | destruction ou échec |
| **bleu** `--accent` | sélection, focus, progression — jamais un aplat de fond |

Les ombres sont **noires et discrètes**. Une ombre colorée attire l'œil sur un
bord, c'est-à-dire nulle part.

## Rayons

Trois valeurs, cohérentes entre elles : **10 px** pour un contrôle, **14 px**
pour une carte, **20 px** pour une boîte de dialogue. Un rayon qui varie sans
règle est ce qui donne l'impression d'un assemblage.

## Mouvement

Deux courbes seulement. `--ease` pour ce qui répond au doigt — court, net.
`--ease-out` pour ce qui entre en scène — plus long, décéléré. Un mouvement
uniforme partout paraît mécanique.

Les entrées durent 0,34 à 0,42 s : assez pour être perçues, jamais assez pour
faire attendre.

## Typographie

Titres **resserrés** (`letter-spacing` négatif) et **graisse forte** (620-650).
C'est le détail invisible qui distingue un outil soigné d'une fenêtre système.

Toute valeur qui change en continu est en **chasse fixe**
(`font-variant-numeric: tabular-nums`) : sans cela le chiffre tressaute pendant
qu'on règle, et le regard le suit au lieu de suivre le son.

## Mouvement

Une seule courbe (`--ease`), partout. Trois durées : 0,12 s pour un retour au
clic, 0,18-0,2 s pour un survol, 0,28-0,35 s pour une entrée.

Les animations d'apparition sont **plafonnées** : la cascade des cartes s'arrête
à douze, au-delà l'élégance devient une lenteur.

Rien ne bouge en boucle sauf ce qui signale un état vivant — le voyant
d'enregistrement, la marque, les vumètres.

## Densité

C'est le **blanc qui crée la hiérarchie**, pas les traits de séparation.
Espacement resserré entre éléments d'un même groupe, large entre groupes.

Les grilles ont une **largeur maximale** : au-delà, l'œil perd la relation entre
les cartes sur un écran large.

## Ce qui trahit un prototype

Corrigé, à ne pas réintroduire :

- **Les barres de défilement de Windows**, larges et claires, qui signalent
  immédiatement une interface web posée dans une fenêtre.
- **Une pastille de curseur teintée**, qui se fond dans son rail. Elle est
  blanche, cerclée d'un halo coloré.
- **Un écran vide qui constate l'absence** au lieu d'expliquer le produit.
- **Un échec silencieux.** Tout état dégradé s'affiche : raccourci refusé,
  écoute indisponible, piste sans son, buffer figé. Un diagnostic invisible a
  déjà tué trois fonctionnalités sans que personne ne le sache.

## Identité des clips

Pas de vignette vidéo : un élément `<video>` par carte garde le fichier ouvert
et mobilise un décodeur, ce qui dégradait la lecture dans l'éditeur.

À la place, une **teinte dérivée du nom du clip** — stable d'une session à
l'autre, différente d'un clip à l'autre, et cantonnée à la famille indigo-violet
de l'accent. On reconnaît sa carte à sa couleur, et la grille cesse d'être une
liste grise.

## Rien ne doit pouvoir pousser l'interface hors du cadre

Trois débordements corrigés, tous dus à du contenu de longueur imprévisible :

- Le **voyant d'état** liste les pistes détectées : tronqué à 42 caractères,
  sinon il repoussait les boutons hors de la fenêtre.
- Le **nom du clip** dans l'éditeur : tronqué, sinon il élargissait le panneau.
- Les **actions du panneau** (Exporter, Supprimer) sont collées en bas et
  restent visibles quand la liste de pistes défile. Sur six pistes, Exporter
  sortait du champ et l'on croyait le bouton absent.

## Adaptation aux tailles

Trois paliers, pas plus :

| | |
|---|---|
| **≥ 1600 px** | la grille respire au lieu de s'étirer |
| **≤ 1080 px** | le mixage passe sous le lecteur — une colonne de 360 px sur 900 px de large ne laisse plus rien à la vidéo |
| **≤ 760 px** | l'en-tête se réorganise, les actions restent atteignables |

## Ce qui reste à faire

- **Persister les vignettes** : elles sont extraites à chaque session. Les
  écrire à côté du clip à la sauvegarde éviterait ce travail répété.
- Mode clair, si un usage hors jeu le justifie un jour.
