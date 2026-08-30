#!/usr/bin/env python3
"""Re-key the local Superwhisper Cohere Q4 checkpoint for mlx-audio-swift.

The tensor data is copied byte-for-byte. Only Safetensors header keys are
renamed from Superwhisper's Swift property paths to the public
mlx-audio-swift Cohere module paths. This keeps the fast 4-bit weights without
checking proprietary model data into this repository.
"""

from __future__ import annotations

import argparse
import json
import shutil
import struct
from pathlib import Path


PREFIXES = (
    ("encoder.preEncode.", "encoder.subsampling."),
    ("encoderDecoderProj.", "bridge_proj."),
    ("logSoftmax.mlpLayer0.", "lm_head."),
    ("transfDecoder.embedding.", "decoder.embedding."),
    ("transfDecoder.decoder.", "decoder.core."),
)

COMPONENTS = {
    "selfAttn": "self_attn",
    "feedForward1": "feed_forward1",
    "feedForward2": "feed_forward2",
    "batchNorm": "batch_norm",
    "linearK": "linear_k",
    "linearOut": "linear_out",
    "linearQ": "linear_q",
    "linearV": "linear_v",
    "linearPos": "linear_pos",
    "firstSubLayer": "first_sub_layer",
    "secondSubLayer": "second_sub_layer",
    "thirdSubLayer": "third_sub_layer",
    "depthwiseConv": "depthwise_conv",
    "pointwiseConv1": "pointwise_conv1",
    "pointwiseConv2": "pointwise_conv2",
    "normConv": "norm_conv",
    "normFeedForward1": "norm_feed_forward1",
    "normFeedForward2": "norm_feed_forward2",
    "normOut": "norm_out",
    "normSelfAtt": "norm_self_att",
    "keyNet": "key_net",
    "queryNet": "query_net",
    "valueNet": "value_net",
    "outProjection": "out_projection",
    "posBiasU": "pos_bias_u",
    "posBiasV": "pos_bias_v",
    "denseIn": "dense_in",
    "denseOut": "dense_out",
    "layerNorm1": "layer_norm_1",
    "layerNorm2": "layer_norm_2",
    "layerNorm3": "layer_norm_3",
    "tokenEmbedding": "token_embedding",
    "layerNorm": "layer_norm",
    "finalLayerNorm": "final_layer_norm",
}

SKIP_SUFFIXES = (".positionEmbedding.posEnc", ".posEnc.pe")


def mapped_key(key: str) -> str:
    for source, target in PREFIXES:
        if key.startswith(source):
            key = target + key[len(source) :]
            break
    return ".".join(COMPONENTS.get(component, component) for component in key.split("."))


def convert(source_dir: Path, output_dir: Path) -> None:
    source_model = source_dir / "model.safetensors"
    if not source_model.is_file():
        raise SystemExit(f"missing {source_model}")

    output_dir.mkdir(parents=True, exist_ok=True)
    for item in source_dir.iterdir():
        if item.name != "model.safetensors" and item.is_file():
            shutil.copy2(item, output_dir / item.name)

    output_model = output_dir / "model.safetensors"
    partial = output_model.with_suffix(".safetensors.partial")
    with source_model.open("rb") as src:
        (header_size,) = struct.unpack("<Q", src.read(8))
        header = json.loads(src.read(header_size))
        data_start = 8 + header_size
        metadata = header.get("__metadata__")
        tensors = []
        cursor = 0
        for key, value in header.items():
            if key == "__metadata__" or key.endswith(SKIP_SUFFIXES):
                continue
            renamed_key = mapped_key(key)
            old_start, old_end = value["data_offsets"]
            size = old_end - old_start
            updated = dict(value)
            updated["data_offsets"] = [cursor, cursor + size]
            tensors.append((renamed_key, updated, old_start, size))
            cursor += size

        renamed = {key: value for key, value, _, _ in tensors}
        if len(renamed) != len(tensors):
            raise SystemExit("key mapping produced duplicate Safetensors keys")
        if metadata is not None:
            renamed["__metadata__"] = metadata

        encoded = json.dumps(renamed, separators=(",", ":")).encode("utf-8")
        padded_size = (len(encoded) + 7) & ~7
        encoded += b" " * (padded_size - len(encoded))

        with partial.open("wb") as dst:
            dst.write(struct.pack("<Q", len(encoded)))
            dst.write(encoded)
            for _, _, old_start, size in tensors:
                src.seek(data_start + old_start)
                remaining = size
                while remaining:
                    block = src.read(min(16 * 1024 * 1024, remaining))
                    if not block:
                        raise SystemExit("unexpected end of Safetensors data")
                    dst.write(block)
                    remaining -= len(block)

    partial.replace(output_model)
    source_keys = [key for key in header if key != "__metadata__"]
    changed = sum(1 for key in source_keys if mapped_key(key) != key)
    skipped = len(source_keys) - len(tensors)
    print(f"converted {len(tensors)} tensors ({changed} renamed, {skipped} skipped)")
    print(output_dir)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("source", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    convert(args.source.expanduser(), args.output.expanduser())


if __name__ == "__main__":
    main()
