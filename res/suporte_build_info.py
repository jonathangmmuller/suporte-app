#!/usr/bin/env python3
"""Metadado da build do app de suporte, ligado ao digest de cada instalador.

Gera `<instalador>.build-info.json` (enviado ao release junto com o instalador). O
`publicar-app-suporte.sh` só assina o manifesto se, para cada arquivo, este metadado existir e
bater com o digest do GitHub, a marca, a variante, a versão e a tag.

Falha (exit != 0) se tag, ambiente do CI e binário divergirem:
  - tag `v<x.y.z>-<marca>[-cliente]`; `-cliente` ⇔ `SUPORTE_SOMENTE_ENTRADA=1`;
  - marca da tag = marca de `SUPORTE_APP_NAME`;
  - relatório `--suporte-build-info` do binário (marca, variante, incoming_only, versão) igual.

Uso:
  suporte_build_info.py --tag TAG (--relatorio ARQ | --sem-binario) INSTALADOR...
"""
import argparse
import hashlib
import json
import os
import re
import sys

MARCAS = {"SuporteVancin": "vancin", "SuporteTalkInCloud": "talkincloud"}
CABECALHO = "suporte-build-info v1"


def falha(msg):
    sys.exit(f"suporte_build_info: {msg}")


def da_tag(tag):
    m = re.fullmatch(r"v(\d+\.\d+\.\d+)-(vancin|talkincloud)(-cliente)?", tag)
    if not m:
        falha(f"tag fora do formato v<x.y.z>-<marca>[-cliente]: {tag!r}")
    return m.group(2), ("cliente" if m.group(3) else "gerencia"), m.group(1)


def do_ambiente():
    nome = os.environ.get("SUPORTE_APP_NAME", "")
    if nome not in MARCAS:
        falha(f"SUPORTE_APP_NAME desconhecido: {nome!r}")
    se = os.environ.get("SUPORTE_SOMENTE_ENTRADA", "0")
    if se not in ("0", "1", ""):
        falha(f"SUPORTE_SOMENTE_ENTRADA inválido: {se!r}")
    return MARCAS[nome], ("cliente" if se == "1" else "gerencia")


def do_relatorio(caminho):
    try:
        linhas = open(caminho, encoding="utf-8").read().splitlines()
    except OSError as e:
        falha(f"binário não gerou o relatório ({e})")
    if not linhas or linhas[0].strip() != CABECALHO:
        falha("relatório sem cabeçalho")
    campos = {}
    for l in linhas[1:]:
        l = l.strip()
        if not l:
            continue
        k, sep, v = l.partition("=")
        if not sep or k in campos:
            falha(f"linha inválida no relatório: {l!r}")
        campos[k] = v
    try:
        r = campos["brand"], campos["variant"], campos["incoming_only"], campos["version"]
    except KeyError as e:
        falha(f"relatório sem {e}")
    if (r[1] == "cliente") != (r[2] == "1") or r[2] not in ("0", "1"):
        falha(f"relatório incoerente: variante {r[1]} com incoming_only={r[2]}")
    return r[0], r[1], r[3]


def sha256(caminho):
    h = hashlib.sha256()
    with open(caminho, "rb") as f:
        for b in iter(lambda: f.read(1 << 20), b""):
            h.update(b)
    return h.hexdigest()


def main(argv=None):
    ap = argparse.ArgumentParser()
    ap.add_argument("--tag", required=True)
    g = ap.add_mutually_exclusive_group(required=True)
    g.add_argument("--relatorio", help="saída de `<app> --suporte-build-info`")
    g.add_argument("--sem-binario", action="store_true",
                   help="só ambiente + tag (binário não executável neste job)")
    ap.add_argument("instaladores", nargs="+")
    a = ap.parse_args(argv)

    marca, variante, versao = da_tag(a.tag)
    marca_env, variante_env = do_ambiente()
    if marca_env != marca:
        falha(f"tag é da marca {marca}, a build é {marca_env}")
    if variante_env != variante:
        falha(f"tag é {variante}, a build foi compilada como {variante_env} "
              "(SUPORTE_SOMENTE_ENTRADA)")
    origem = "ambiente"
    if a.relatorio:
        rb = do_relatorio(a.relatorio)
        if rb != (marca, variante, versao):
            falha(f"binário é {':'.join(rb)}, tag pede {marca}:{variante}:{versao}")
        origem = "binario"

    for inst in a.instaladores:
        if not os.path.isfile(inst):
            falha(f"instalador ausente: {inst}")
        info = dict(schema=1, file=os.path.basename(inst), sha256=sha256(inst),
                    size=os.path.getsize(inst), brand=marca, variant=variante,
                    app_version=versao, tag=a.tag,
                    commit=os.environ.get("GITHUB_SHA", ""), origem=origem)
        with open(inst + ".build-info.json", "w", encoding="utf-8") as f:
            f.write(json.dumps(info, ensure_ascii=False, indent=1) + "\n")
        print(f"{info['file']}: {marca}/{variante} {versao} sha256={info['sha256']} ({origem})")


if __name__ == "__main__":
    main()
