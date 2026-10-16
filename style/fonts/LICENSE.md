# Font licenses

`migu1c-regular` and `migu2m-regular` are glyph ranges (SDF PBF) generated from
the Migu 1C and Migu 2M fonts, copied from
[osmfj/tileserver-gl-site](https://github.com/osmfj/tileserver-gl-site), which
serves them on tile.openstreetmap.jp.

Migu is by Itou Hiroki and combines two fonts:

- M+ FONTS — Copyright (C) 2002-2020 M+ FONTS PROJECT
- IPA Gothic (IPAゴシック) — Copyright (c) Information-technology Promotion
  Agency, Japan (IPA), 2003-2011

Migu is licensed under the IPA Font License Agreement v1.0, as its README says.
The texts that come with the Migu package are in `licenses/`, unchanged:

| File | What it is | From |
| --- | --- | --- |
| `licenses/migu-README.txt` | Migu's README, including its license statement | [itouhiro/mixfont-mplus-ipa](https://github.com/itouhiro/mixfont-mplus-ipa) `mplus_outline_fonts/mig.d/release/` |
| `licenses/ipag00303/IPA_Font_License_Agreement_v1.0.txt` | IPA Font License Agreement v1.0 | same repository, `mplus_outline_fonts/mig.d/sfd.d/` |
| `licenses/ipag00303/Readme_ipag00303.txt` | IPA Gothic's README | same repository, `mplus_outline_fonts/mig.d/sfd.d/` |
| `licenses/mplus/LICENSE_E`, `LICENSE_J` | M+ FONTS license | M+ TESTFLIGHT 063, via Debian's [`fonts-mplus` 063-3](https://sources.debian.org/src/fonts-mplus/063-3/) |
| `licenses/mplus/README_E`, `README_J` | M+ FONTS README | same |

Only the `.pbf` files are compiled into the app (see `build.rs`); these texts
are not. Anything that distributes the app should carry them alongside it.
