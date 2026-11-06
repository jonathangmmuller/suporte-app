"""Troca o nome dos atalhos do MSI pelo nome de exibição da marca (com espaço).
Uso, depois do preprocess.py: python brand_shortcuts.py <NomeTecnico> "<Nome Exibido>"."""
import re
import sys
from pathlib import Path

app, display = sys.argv[1], sys.argv[2]
ids = ("SC_Client", "SC_Uninstall")
for f in Path(__file__).parent.joinpath("Package/Language").glob("*.wxl"):
    text = f.read_text(encoding="utf-8")
    for sid in ids:
        text = re.sub(
            rf'(<String Id="{sid}" Value=")([^"]*)(")',
            lambda m: m.group(1) + m.group(2).replace(app, display) + m.group(3),
            text,
        )
    f.write_text(text, encoding="utf-8")
print(f"atalhos do MSI: {app} -> {display}")
