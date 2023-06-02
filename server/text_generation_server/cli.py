import os
from typing import Optional

import typer

from pathlib import Path

from text_generation_server import server, utils
from text_generation_server.utils.hub import get_model_path, local_weight_files

app = typer.Typer()


@app.command()
def serve(
    model_name: str,
    deployment_framework: str,
    dtype_str: str,
    revision: Optional[str] = None,
    sharded: bool = False,
    uds_path: Path = "/tmp/text-generation",
):
    if sharded:
        assert (
            os.getenv("RANK", None) is not None
        ), "RANK must be set when sharded is True"
        assert (
            os.getenv("WORLD_SIZE", None) is not None
        ), "WORLD_SIZE must be set when sharded is True"
        assert (
            os.getenv("MASTER_ADDR", None) is not None
        ), "MASTER_ADDR must be set when sharded is True"
        assert (
            os.getenv("MASTER_PORT", None) is not None
        ), "MASTER_PORT must be set when sharded is True"

    server.serve(model_name, revision, deployment_framework, dtype_str, sharded, uds_path)


@app.command()
def download_weights(
    model_name: str,
    revision: Optional[str] = None,
    token: Optional[str] = None,
    extension: str = ".safetensors",
):
    utils.download_weights(model_name, extension.split(","), revision=revision, auth_token=token)


@app.command()
def convert_to_onnx(
    model_name: str,
    target_model_name: str,
    revision: Optional[str] = None,
    merge_graphs: Optional[bool] = False,
    optimize: Optional[bool] = False,
    provider: Optional[str] = None,
):
    # Onnx currently isn't included in image used for CI tests
    from text_generation_server.utils import onnx

    onnx.convert_model(
        model_name, target_model_name,
        revision=revision,
        merge_graphs=merge_graphs,
        optimize=optimize,
        provider=provider
    )


@app.command()
def convert_to_safetensors(
    model_name: str,
    revision: Optional[str] = None,
):
    # Get local pytorch file paths
    model_path = get_model_path(model_name, revision)
    local_pt_files = local_weight_files(model_path, ".bin")
    local_pt_files = [Path(f) for f in local_pt_files]

    # Safetensors final filenames
    local_st_files = [
        p.parent / f"{p.stem.lstrip('pytorch_')}.safetensors"
        for p in local_pt_files
    ]

    # Convert pytorch weights to safetensors
    utils.convert_files(local_pt_files, local_st_files)


if __name__ == "__main__":
    app()
