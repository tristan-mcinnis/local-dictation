# Third-party notices

Local Dictation is free and open source, dual-licensed under MIT OR Apache-2.0
(`LICENSE-MIT`, `LICENSE-APACHE`), Copyright (c) 2026 Tristan McInnis.

It is built on the work of the people and projects below. Thank you. Each
licence was checked against the dependency's own licence file (the crate source
in `~/.cargo/registry`, the SwiftPM checkout in `native/cohere-mlx/.build`, or
the upstream repository). `scripts/build-app.sh` copies this file and both
licence texts into `Local Dictation.app/Contents/Resources`.

## Models

No model weights are committed to this repository. `scripts/download-models.sh`
downloads them, or the user supplies them.

### Cohere Transcribe 03-2026, by Cohere Labs

The speech-to-text model.

- Model: <https://huggingface.co/CohereLabs/cohere-transcribe-03-2026>
- Licence: Apache-2.0 (stated on the model card). Hugging Face asks you to share
  contact details before it shows the original files; that is an access step,
  not a licence term.
- 4-bit MLX conversion used by `download-models.sh`:
  <https://huggingface.co/beshkenadze/cohere-transcribe-03-2026-mlx-4bit>
  by beshkenadze (affine 4-bit, group size 64, Apache-2.0, pinned to revision
  `104bc4391b5b1a12b040859793d7148525e1a08c`). The conversion repository ships
  no licence file, so the script saves the Apache-2.0 text beside the weights.
- Optional local source: `scripts/models/convert_superwhisper_cohere.py`
  re-keys the Safetensors header names of a Cohere checkpoint that a local
  Superwhisper install already holds. Tensor bytes are copied unchanged. Either
  way the result is a derived work of the Apache-2.0 model and keeps its licence
  and attribution.

### "S1-mini" by "Superwhisper"

The cleanup model. Its licence requires that exact name and capitalization
wherever the model is used.

- Model: <https://huggingface.co/superwhisper/s1-mini>
- GGUF builds: <https://huggingface.co/superwhisper/s1-mini-GGUF>
- Licence: Apache-2.0 with an additional naming term. Copyright 2026
  Superwhisper. The model's own LICENSE and NOTICE are saved beside the weights
  by `download-models.sh` and by the app's Get model button.
- Base model: Qwen3-0.6B, Copyright 2024 Alibaba Cloud, Apache-2.0,
  <https://huggingface.co/Qwen/Qwen3-0.6B>

Local Dictation loads the released weights unmodified and supplies its own
prompts and guards.

### Voice activity detection

There is no VAD model. `src/vad.rs` is a small energy-based detector written
for this project.

## Native speech helper (Swift, `native/cohere-mlx`)

Pinned in `native/cohere-mlx/Package.resolved` and linked into
`cohere-mlx-bridge`.

| Project | Version | Licence | Copyright |
| --- | --- | --- | --- |
| [mlx-audio-swift](https://github.com/Blaizzy/mlx-audio-swift) (Cohere Transcribe port, MLXAudioSTT) | cae704f | MIT | Copyright (c) 2025 Prince Canuma |
| [mlx-swift](https://github.com/ml-explore/mlx-swift) | 0.31.4 | MIT | Copyright (c) 2023 ml-explore |
| [MLX](https://github.com/ml-explore/mlx), vendored by mlx-swift | in mlx-swift 0.31.4 | MIT | Copyright © 2023 Apple Inc. |
| [mlx-c](https://github.com/ml-explore/mlx-c), vendored by mlx-swift | in mlx-swift 0.31.4 | MIT | Copyright (c) 2023 ml-explore |
| [metal-cpp](https://developer.apple.com/metal/cpp/), vendored by mlx-swift | in mlx-swift 0.31.4 | Apache-2.0 | Apple Inc. (LICENSE.txt in the vendored copy) |
| [{fmt}](https://github.com/fmtlib/fmt), vendored by mlx-swift | in mlx-swift 0.31.4 | MIT | Copyright (c) 2012 - present, Victor Zverovich and {fmt} contributors |
| [JSON for Modern C++](https://github.com/nlohmann/json), vendored by mlx-swift | in mlx-swift 0.31.4 | MIT | Copyright (c) 2013-2022 Niels Lohmann |
| [mlx-swift-lm](https://github.com/ml-explore/mlx-swift-lm) | 3.31.4 | MIT | Copyright (c) 2024 ml-explore |
| [swift-transformers](https://github.com/huggingface/swift-transformers) | 1.3.3 | Apache-2.0 | the swift-transformers authors (Hugging Face) |
| [swift-huggingface](https://github.com/huggingface/swift-huggingface) | 0.9.0 | Apache-2.0 | the swift-huggingface authors (Hugging Face) |
| [swift-jinja](https://github.com/huggingface/swift-jinja) | 2.4.2 | Apache-2.0 | the swift-jinja authors (Hugging Face) |
| [swift-collections](https://github.com/apple/swift-collections) | 1.6.0 | Apache-2.0 | the Swift Collections project authors (Apple) |
| [swift-numerics](https://github.com/apple/swift-numerics) | 1.1.1 | Apache-2.0 | the Swift Numerics project authors (Apple) |
| [swift-crypto](https://github.com/apple/swift-crypto) | 4.5.1 | Apache-2.0 | Copyright 2019 The SwiftCrypto Project (from its NOTICE.txt) |
| [swift-asn1](https://github.com/apple/swift-asn1) | 1.7.1 | Apache-2.0 | Copyright 2022 The SwiftASN1 Project (from its NOTICE.txt) |
| [swift-syntax](https://github.com/swiftlang/swift-syntax) | 603.0.2 | Apache-2.0 | the Swift project authors; build-time only (macros), not in the binary |
| [EventSource](https://github.com/mattt/EventSource) | 1.5.1 | MIT | Copyright 2025 Mattt |
| [yyjson](https://github.com/ibireme/yyjson) | 0.12.0 | MIT | Copyright (c) 2020 YaoYuan |

swift-crypto's NOTICE.txt also records that it contains Google wycheproof test
vectors (Apache-2.0) and files derived from SwiftNIO (Apache-2.0).

## llama.cpp

The cleanup model runs on [llama.cpp](https://github.com/ggml-org/llama.cpp)
and ggml (MIT, Copyright (c) 2023-2026 The ggml authors), compiled from the
copy vendored in the `llama-cpp-sys-2` crate. Its `common` library brings in
[JSON for Modern C++](https://github.com/nlohmann/json) (MIT, Copyright 2013 -
2025 Niels Lohmann). The Rust bindings are
[llama-cpp-rs](https://github.com/utilityai/llama-cpp-rs) by utilityai (MIT OR
Apache-2.0).

## SQLite

`rusqlite` compiles the bundled [SQLite](https://sqlite.org) amalgamation
(via `libsqlite3-sys`). SQLite is in the public domain.

## Rust crates

These 71 crates are compiled into the release binary with
`cargo build --features full` on Apple Silicon (from
`cargo tree --features full -e normal --target aarch64-apple-darwin`). Where a
crate offers a choice, Local Dictation uses it under MIT or Apache-2.0. No
copyleft licence is present.

| Crate | Version | Licence (as declared) | Copyright line in its licence file | Checked against |
| --- | --- | --- | --- | --- |
| accessibility-sys | 0.2.0 | MIT / Apache-2.0 | the accessibility-sys authors | eiz/accessibility (LICENSE-MIT, LICENSE-APACHE) |
| arboard | 3.6.1 | MIT OR Apache-2.0 | Copyright (c) 2022 The Arboard contributors | licence files in the crate source |
| base64 | 0.22.1 | MIT OR Apache-2.0 | Copyright (c) 2015 Alice Maz | licence files in the crate source |
| bitflags | 1.3.2 | MIT/Apache-2.0 | Copyright (c) 2014 The Rust Project Developers | licence files in the crate source |
| bitflags | 2.11.1 | MIT OR Apache-2.0 | Copyright (c) 2014 The Rust Project Developers | licence files in the crate source |
| block2 | 0.6.2 | MIT | the block2 authors | madsmtm/objc2 (LICENSE.md: MIT) |
| cfg-if | 1.0.4 | MIT OR Apache-2.0 | Copyright (c) 2014 Alex Crichton | licence files in the crate source |
| core-foundation | 0.10.1 | MIT OR Apache-2.0 | Copyright (c) 2012-2013 Mozilla Foundation | licence files in the crate source |
| core-foundation-sys | 0.8.7 | MIT OR Apache-2.0 | Copyright (c) 2012-2013 Mozilla Foundation | licence files in the crate source |
| core-graphics | 0.25.0 | MIT OR Apache-2.0 | Copyright (c) 2012-2013 Mozilla Foundation | licence files in the crate source |
| core-graphics-types | 0.2.0 | MIT OR Apache-2.0 | Copyright (c) 2012-2013 Mozilla Foundation | licence files in the crate source |
| coreaudio-rs | 0.11.3 | MIT/Apache-2.0 | Copyright (c) 2015 | licence files in the crate source |
| coreaudio-sys | 0.2.17 | MIT | Copyright (c) 2015 | licence files in the crate source |
| cpal | 0.15.3 | Apache-2.0 | the cpal authors | licence files in the crate source |
| crossbeam-utils | 0.8.21 | MIT OR Apache-2.0 | Copyright (c) 2019 The Crossbeam Project Developers | licence files in the crate source |
| dasp_sample | 0.11.0 | MIT OR Apache-2.0 | the dasp_sample authors | RustAudio/dasp (LICENSE-MIT, LICENSE-APACHE) |
| dispatch2 | 0.3.1 | Zlib OR Apache-2.0 OR MIT | the dispatch2 authors | madsmtm/objc2 (LICENSE.md) |
| encoding_rs | 0.8.35 | (Apache-2.0 OR MIT) AND BSD-3-Clause | Copyright © WHATWG (Apple, Google, Mozilla, Microsoft). | licence files in the crate source |
| enumflags2 | 0.7.12 | MIT OR Apache-2.0 | Copyright (c) 2017-2023 Maik Klein, Maja Kądziołka; Copyright 2017-2023 Maik Klein, Maja Kądziołka | licence files in the crate source |
| enumflags2_derive | 0.7.12 | MIT OR Apache-2.0 | Copyright (c) 2017 Maik Klein | licence files in the crate source |
| eyre | 0.6.12 | MIT OR Apache-2.0 | the eyre authors | licence files in the crate source |
| fallible-iterator | 0.3.0 | MIT/Apache-2.0 | Copyright (c) 2015 The rust-openssl-verify Developers | licence files in the crate source |
| fallible-streaming-iterator | 0.1.9 | MIT/Apache-2.0 | Copyright (c) 2016 The fallible-streaming-iterator Developers | licence files in the crate source |
| foldhash | 0.2.0 | Zlib | Copyright (c) 2024 Orson Peters | licence files in the crate source |
| foreign-types | 0.5.0 | MIT/Apache-2.0 | Copyright (c) 2017 The foreign-types Developers | licence files in the crate source |
| foreign-types-macros | 0.2.3 | MIT/Apache-2.0 | Copyright (c) 2017 The foreign-types Developers | licence files in the crate source |
| foreign-types-shared | 0.3.1 | MIT/Apache-2.0 | Copyright (c) 2017 The foreign-types Developers | licence files in the crate source |
| hashbrown | 0.16.1 | MIT OR Apache-2.0 | Copyright (c) 2016 Amanieu d'Antras | licence files in the crate source |
| hashlink | 0.11.0 | MIT OR Apache-2.0 | the hashlink authors | licence files in the crate source |
| hound | 3.5.1 | Apache-2.0 | the hound authors | licence files in the crate source |
| indenter | 0.3.4 | MIT OR Apache-2.0 | the indenter authors | licence files in the crate source |
| itoa | 1.0.18 | MIT OR Apache-2.0 | the itoa authors | licence files in the crate source |
| libc | 0.2.186 | MIT OR Apache-2.0 | Copyright (c) The Rust Project Developers | licence files in the crate source |
| libsqlite3-sys | 0.37.0 | MIT | Copyright (c) 2014 The rusqlite developers | licence files in the crate source |
| llama-cpp-2 | 0.1.146 | MIT OR Apache-2.0 | the llama-cpp-2 authors | utilityai/llama-cpp-rs (LICENSE-MIT, LICENSE-APACHE) |
| llama-cpp-sys-2 | 0.1.146 | MIT OR Apache-2.0 | the llama-cpp-sys-2 authors | utilityai/llama-cpp-rs (LICENSE-MIT, LICENSE-APACHE) |
| mach2 | 0.4.3 | BSD-2-Clause OR MIT OR Apache-2.0 | Copyright (c) 2019 Nick Fitzgerald, 2021 Yuki Okushi | licence files in the crate source |
| memchr | 2.8.0 | Unlicense OR MIT | Copyright (c) 2015 Andrew Gallant | licence files in the crate source |
| objc2 | 0.6.4 | MIT | the objc2 authors | madsmtm/objc2 (LICENSE.md) |
| objc2-app-kit | 0.3.2 | Zlib OR Apache-2.0 OR MIT | the objc2-app-kit authors | madsmtm/objc2 (LICENSE.md) |
| objc2-cloud-kit | 0.3.2 | Zlib OR Apache-2.0 OR MIT | the objc2-cloud-kit authors | madsmtm/objc2 (LICENSE.md) |
| objc2-core-data | 0.3.2 | Zlib OR Apache-2.0 OR MIT | the objc2-core-data authors | madsmtm/objc2 (LICENSE.md) |
| objc2-core-foundation | 0.3.2 | Zlib OR Apache-2.0 OR MIT | the objc2-core-foundation authors | madsmtm/objc2 (LICENSE.md) |
| objc2-core-graphics | 0.3.2 | Zlib OR Apache-2.0 OR MIT | the objc2-core-graphics authors | madsmtm/objc2 (LICENSE.md) |
| objc2-core-image | 0.3.2 | Zlib OR Apache-2.0 OR MIT | the objc2-core-image authors | madsmtm/objc2 (LICENSE.md) |
| objc2-core-text | 0.3.2 | Zlib OR Apache-2.0 OR MIT | the objc2-core-text authors | madsmtm/objc2 (LICENSE.md) |
| objc2-core-video | 0.3.2 | Zlib OR Apache-2.0 OR MIT | the objc2-core-video authors | madsmtm/objc2 (LICENSE.md) |
| objc2-encode | 4.1.0 | MIT | the objc2-encode authors | madsmtm/objc2 (LICENSE.md) |
| objc2-foundation | 0.3.2 | MIT | the objc2-foundation authors | madsmtm/objc2 (LICENSE.md) |
| objc2-quartz-core | 0.3.2 | Zlib OR Apache-2.0 OR MIT | the objc2-quartz-core authors | madsmtm/objc2 (LICENSE.md) |
| once_cell | 1.21.4 | MIT OR Apache-2.0 | the once_cell authors | licence files in the crate source |
| pin-project-lite | 0.2.17 | Apache-2.0 OR MIT | the pin-project-lite authors | licence files in the crate source |
| proc-macro2 | 1.0.106 | MIT OR Apache-2.0 | the proc-macro2 authors | licence files in the crate source |
| quote | 1.0.45 | MIT OR Apache-2.0 | the quote authors | licence files in the crate source |
| ringbuf | 0.4.8 | MIT/Apache-2.0 | Copyright (c) 2019 Alexey Gerasev | licence files in the crate source |
| rusqlite | 0.39.0 | MIT | Copyright (c) 2014 The rusqlite developers | licence files in the crate source |
| serde | 1.0.228 | MIT OR Apache-2.0 | the serde authors | licence files in the crate source |
| serde_core | 1.0.228 | MIT OR Apache-2.0 | the serde_core authors | licence files in the crate source |
| serde_derive | 1.0.228 | MIT OR Apache-2.0 | the serde_derive authors | licence files in the crate source |
| serde_json | 1.0.150 | MIT OR Apache-2.0 | the serde_json authors | licence files in the crate source |
| smallvec | 1.15.1 | MIT OR Apache-2.0 | Copyright (c) 2018 The Servo Project Developers | licence files in the crate source |
| syn | 2.0.117 | MIT OR Apache-2.0 | the syn authors | licence files in the crate source |
| thiserror | 2.0.18 | MIT OR Apache-2.0 | the thiserror authors | licence files in the crate source |
| thiserror-impl | 2.0.18 | MIT OR Apache-2.0 | the thiserror-impl authors | licence files in the crate source |
| tokio | 1.52.3 | MIT | Copyright (c) Tokio Contributors | licence files in the crate source |
| tokio-macros | 2.7.0 | MIT | Copyright (c) 2019 Yoshua Wuyts; Copyright (c) Tokio Contributors | licence files in the crate source |
| tracing | 0.1.44 | MIT | Copyright (c) 2019 Tokio Contributors | licence files in the crate source |
| tracing-attributes | 0.1.31 | MIT | Copyright (c) 2019 Tokio Contributors | licence files in the crate source |
| tracing-core | 0.1.36 | MIT | Copyright (c) 2019 Tokio Contributors | licence files in the crate source |
| unicode-ident | 1.0.24 | (MIT OR Apache-2.0) AND Unicode-3.0 | Copyright © 1991-2023 Unicode, Inc. | licence files in the crate source |
| zmij | 1.0.21 | MIT | the zmij authors | licence files in the crate source |

## Sparkle (release builds only)

[Sparkle](https://github.com/sparkle-project/Sparkle) 2 is not committed or
linked by default. `scripts/build-app.sh --release` embeds it from
`SPARKLE_FRAMEWORK` at build time. Sparkle is MIT (Copyright (c) 2006-2013 Andy
Matuschak and others); its LICENSE file also carries the bsdiff (BSD-2-Clause),
sais-lite (MIT), ed25519 (Zlib) and SUSignatureVerifier (BSD) notices, and a
release build that embeds Sparkle must ship that file too. The release
pipeline's Ed25519 signer uses Apple's CryptoKit, part of macOS.

## Design tokens

`src/menubar/house_theme.rs` is generated from the House design system tokens.
That design system is the author's own work and is used here under this
repository's licence.

## Sounds

The state cues and the start sounds are original synthesis, generated by
`scripts/audio/generate_cues.py`. Twelve of the twenty completion sounds are
original synthesis too.

### Kenney Vleugels interface and impact sounds (CC0)

Eight completion sounds in `resources/completion_sounds/` are by Kenney
Vleugels (<https://kenney.nl>), released under Creative Commons Zero (CC0 1.0,
<https://creativecommons.org/publicdomain/zero/1.0/>). No attribution is
required; it is given gladly.

| Completion sound | Original file | Canonical pack |
| --- | --- | --- |
| `kenney-tight-click` | `click2.ogg` | [UI Audio](https://kenney.nl/assets/ui-audio) |
| `kenney-bong-impact` | `bong_001.ogg` | [Interface Sounds](https://kenney.nl/assets/interface-sounds) |
| `kenney-drop-impact` | `drop_001.ogg` | [Interface Sounds](https://kenney.nl/assets/interface-sounds) |
| `kenney-soft-impact` | `impactSoft_medium_000.ogg` | [Impact Sounds](https://kenney.nl/assets/impact-sounds) |
| `kenney-heavy-wood-impact` | `impactWood_heavy_000.ogg` | [Impact Sounds](https://kenney.nl/assets/impact-sounds) |
| `kenney-confirmation-1` | `confirmation_001.ogg` | [Interface Sounds](https://kenney.nl/assets/interface-sounds) |
| `kenney-confirmation-2` | `confirmation_002.ogg` | [Interface Sounds](https://kenney.nl/assets/interface-sounds) |
| `kenney-confirmation-3` | `confirmation_003.ogg` | [Interface Sounds](https://kenney.nl/assets/interface-sounds) |

See `resources/completion_sounds/README.md` for the full id and label map.

## Licence texts

### MIT License

Each MIT component above is distributed under this text, with the copyright
line shown for it in the tables above.

```text
Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

### BSD-3-Clause (encoding_rs, WHATWG data)

```text
Copyright © WHATWG (Apple, Google, Mozilla, Microsoft).

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice, this
   list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

3. Neither the name of the copyright holder nor the names of its
   contributors may be used to endorse or promote products derived from
   this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

### Zlib (foldhash)

```text
Copyright (c) 2024 Orson Peters

This software is provided 'as-is', without any express or implied warranty. In
no event will the authors be held liable for any damages arising from the use of
this software.

Permission is granted to anyone to use this software for any purpose, including
commercial applications, and to alter it and redistribute it freely, subject to
the following restrictions:

1. The origin of this software must not be misrepresented; you must not claim
    that you wrote the original software. If you use this software in a product,
    an acknowledgment in the product documentation would be appreciated but is
    not required.

2. Altered source versions must be plainly marked as such, and must not be
    misrepresented as being the original software.

3. This notice may not be removed or altered from any source distribution.
```

### Unicode License v3 (unicode-ident data)

```text
UNICODE LICENSE V3

COPYRIGHT AND PERMISSION NOTICE

Copyright © 1991-2023 Unicode, Inc.

NOTICE TO USER: Carefully read the following legal agreement. BY
DOWNLOADING, INSTALLING, COPYING OR OTHERWISE USING DATA FILES, AND/OR
SOFTWARE, YOU UNEQUIVOCALLY ACCEPT, AND AGREE TO BE BOUND BY, ALL OF THE
TERMS AND CONDITIONS OF THIS AGREEMENT. IF YOU DO NOT AGREE, DO NOT
DOWNLOAD, INSTALL, COPY, DISTRIBUTE OR USE THE DATA FILES OR SOFTWARE.

Permission is hereby granted, free of charge, to any person obtaining a
copy of data files and any associated documentation (the "Data Files") or
software and any associated documentation (the "Software") to deal in the
Data Files or Software without restriction, including without limitation
the rights to use, copy, modify, merge, publish, distribute, and/or sell
copies of the Data Files or Software, and to permit persons to whom the
Data Files or Software are furnished to do so, provided that either (a)
this copyright and permission notice appear with all copies of the Data
Files or Software, or (b) this copyright and permission notice appear in
associated Documentation.

THE DATA FILES AND SOFTWARE ARE PROVIDED "AS IS", WITHOUT WARRANTY OF ANY
KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT OF
THIRD PARTY RIGHTS.

IN NO EVENT SHALL THE COPYRIGHT HOLDER OR HOLDERS INCLUDED IN THIS NOTICE
BE LIABLE FOR ANY CLAIM, OR ANY SPECIAL INDIRECT OR CONSEQUENTIAL DAMAGES,
OR ANY DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS,
WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION,
ARISING OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THE DATA
FILES OR SOFTWARE.

Except as contained in this notice, the name of a copyright holder shall
not be used in advertising or otherwise to promote the sale, use or other
dealings in these Data Files or Software without prior written
authorization of the copyright holder.
```

### Apache License 2.0

Covers Cohere Transcribe, "S1-mini" by "Superwhisper", Qwen3-0.6B, the Apache
Swift packages, metal-cpp, cpal, hound, and the crates offered under Apache-2.0.

```text
                                 Apache License
                           Version 2.0, January 2004
                        http://www.apache.org/licenses/

   TERMS AND CONDITIONS FOR USE, REPRODUCTION, AND DISTRIBUTION

   1. Definitions.

      "License" shall mean the terms and conditions for use, reproduction,
      and distribution as defined by Sections 1 through 9 of this document.

      "Licensor" shall mean the copyright owner or entity authorized by
      the copyright owner that is granting the License.

      "Legal Entity" shall mean the union of the acting entity and all
      other entities that control, are controlled by, or are under common
      control with that entity. For the purposes of this definition,
      "control" means (i) the power, direct or indirect, to cause the
      direction or management of such entity, whether by contract or
      otherwise, or (ii) ownership of fifty percent (50%) or more of the
      outstanding shares, or (iii) beneficial ownership of such entity.

      "You" (or "Your") shall mean an individual or Legal Entity
      exercising permissions granted by this License.

      "Source" form shall mean the preferred form for making modifications,
      including but not limited to software source code, documentation
      source, and configuration files.

      "Object" form shall mean any form resulting from mechanical
      transformation or translation of a Source form, including but
      not limited to compiled object code, generated documentation,
      and conversions to other media types.

      "Work" shall mean the work of authorship, whether in Source or
      Object form, made available under the License, as indicated by a
      copyright notice that is included in or attached to the work
      (an example is provided in the Appendix below).

      "Derivative Works" shall mean any work, whether in Source or Object
      form, that is based on (or derived from) the Work and for which the
      editorial revisions, annotations, elaborations, or other modifications
      represent, as a whole, an original work of authorship. For the purposes
      of this License, Derivative Works shall not include works that remain
      separable from, or merely link (or bind by name) to the interfaces of,
      the Work and Derivative Works thereof.

      "Contribution" shall mean any work of authorship, including
      the original version of the Work and any modifications or additions
      to that Work or Derivative Works thereof, that is intentionally
      submitted to Licensor for inclusion in the Work by the copyright owner
      or by an individual or Legal Entity authorized to submit on behalf of
      the copyright owner. For the purposes of this definition, "submitted"
      means any form of electronic, verbal, or written communication sent
      to the Licensor or its representatives, including but not limited to
      communication on electronic mailing lists, source code control systems,
      and issue tracking systems that are managed by, or on behalf of, the
      Licensor for the purpose of discussing and improving the Work, but
      excluding communication that is conspicuously marked or otherwise
      designated in writing by the copyright owner as "Not a Contribution."

      "Contributor" shall mean Licensor and any individual or Legal Entity
      on behalf of whom a Contribution has been received by Licensor and
      subsequently incorporated within the Work.

   2. Grant of Copyright License. Subject to the terms and conditions of
      this License, each Contributor hereby grants to You a perpetual,
      worldwide, non-exclusive, no-charge, royalty-free, irrevocable
      copyright license to reproduce, prepare Derivative Works of,
      publicly display, publicly perform, sublicense, and distribute the
      Work and such Derivative Works in Source or Object form.

   3. Grant of Patent License. Subject to the terms and conditions of
      this License, each Contributor hereby grants to You a perpetual,
      worldwide, non-exclusive, no-charge, royalty-free, irrevocable
      (except as stated in this section) patent license to make, have made,
      use, offer to sell, sell, import, and otherwise transfer the Work,
      where such license applies only to those patent claims licensable
      by such Contributor that are necessarily infringed by their
      Contribution(s) alone or by combination of their Contribution(s)
      with the Work to which such Contribution(s) was submitted. If You
      institute patent litigation against any entity (including a
      cross-claim or counterclaim in a lawsuit) alleging that the Work
      or a Contribution incorporated within the Work constitutes direct
      or contributory patent infringement, then any patent licenses
      granted to You under this License for that Work shall terminate
      as of the date such litigation is filed.

   4. Redistribution. You may reproduce and distribute copies of the
      Work or Derivative Works thereof in any medium, with or without
      modifications, and in Source or Object form, provided that You
      meet the following conditions:

      (a) You must give any other recipients of the Work or
          Derivative Works a copy of this License; and

      (b) You must cause any modified files to carry prominent notices
          stating that You changed the files; and

      (c) You must retain, in the Source form of any Derivative Works
          that You distribute, all copyright, patent, trademark, and
          attribution notices from the Source form of the Work,
          excluding those notices that do not pertain to any part of
          the Derivative Works; and

      (d) If the Work includes a "NOTICE" text file as part of its
          distribution, then any Derivative Works that You distribute must
          include a readable copy of the attribution notices contained
          within such NOTICE file, excluding those notices that do not
          pertain to any part of the Derivative Works, in at least one
          of the following places: within a NOTICE text file distributed
          as part of the Derivative Works; within the Source form or
          documentation, if provided along with the Derivative Works; or,
          within a display generated by the Derivative Works, if and
          wherever such third-party notices normally appear. The contents
          of the NOTICE file are for informational purposes only and
          do not modify the License. You may add Your own attribution
          notices within Derivative Works that You distribute, alongside
          or as an addendum to the NOTICE text from the Work, provided
          that such additional attribution notices cannot be construed
          as modifying the License.

      You may add Your own copyright statement to Your modifications and
      may provide additional or different license terms and conditions
      for use, reproduction, or distribution of Your modifications, or
      for any such Derivative Works as a whole, provided Your use,
      reproduction, and distribution of the Work otherwise complies with
      the conditions stated in this License.

   5. Submission of Contributions. Unless You explicitly state otherwise,
      any Contribution intentionally submitted for inclusion in the Work
      by You to the Licensor shall be under the terms and conditions of
      this License, without any additional terms or conditions.
      Notwithstanding the above, nothing herein shall supersede or modify
      the terms of any separate license agreement you may have executed
      with Licensor regarding such Contributions.

   6. Trademarks. This License does not grant permission to use the trade
      names, trademarks, service marks, or product names of the Licensor,
      except as required for describing the origin of the Work and
      reproducing the content of the NOTICE file.

   7. Disclaimer of Warranty. Unless required by applicable law or
      agreed to in writing, Licensor provides the Work (and each
      Contributor provides its Contributions) on an "AS IS" BASIS,
      WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or
      implied, including, without limitation, any warranties or conditions
      of TITLE, NON-INFRINGEMENT, MERCHANTABILITY, or FITNESS FOR A
      PARTICULAR PURPOSE. You are solely responsible for determining the
      appropriateness of using or redistributing the Work and assume any
      risks associated with Your exercise of permissions under this License.

   8. Limitation of Liability. In no event and under no legal theory,
      whether in tort (including negligence), contract, or otherwise,
      unless required by applicable law (such as deliberate and grossly
      negligent acts) or agreed to in writing, shall any Contributor be
      liable to You for damages, including any direct, indirect, special,
      incidental, or consequential damages of any character arising as a
      result of this License or out of the use or inability to use the
      Work (including but not limited to damages for loss of goodwill,
      work stoppage, computer failure or malfunction, or any and all
      other commercial damages or losses), even if such Contributor
      has been advised of the possibility of such damages.

   9. Accepting Warranty or Additional Liability. While redistributing
      the Work or Derivative Works thereof, You may elect to offer,
      and charge a fee for, acceptance of support, warranty, indemnity,
      or other liability obligations and/or rights consistent with this
      License. However, in accepting such obligations, You may act only
      on Your own behalf and on Your sole responsibility, not on behalf
      of any other Contributor, and only if You agree to indemnify,
      defend, and hold each Contributor harmless for any liability
      incurred by, or claims asserted against, such Contributor by reason
      of your accepting any such warranty or additional liability.

   END OF TERMS AND CONDITIONS
```
