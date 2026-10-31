#!/usr/bin/env python3
"""Vendor the shared <cola-header> fallback CSS as static/cola-header.css (/cola-header.css).

Usage: python3 -I scripts/vendor-header-css.py PATH/TO/cola-header.css

PATH is KubeShelf's built public file (web/dist/branding/v1/cola-header.css after
`npm --prefix web run build`, generated from web/src/cola-header.css). The output has the
same bytes as the GitOps scripts/branding/cola-header.vendored.css made from the same input.
The copy is only the no-JS/provider-failure fallback, so it may lag the provider; the live
header styles itself from the provider script. Afterwards update compat/static-assets.sha256.
"""
from pathlib import Path
import hashlib, sys

OUT = Path(__file__).resolve().parent.parent / 'static' / 'cola-header.css'

if len(sys.argv) != 2:
    sys.exit(__doc__)
css = Path(sys.argv[1]).read_text()
# The scope keeps an old copy from overriding a newer script's shadow styles.
if '</' in css or '@import' in css or 'cola-header:not(:defined)' not in css:
    sys.exit('Refusing: expected the scoped cola-header:not(:defined) fallback CSS from build-plum.cjs.')
digest = hashlib.sha256(css.encode()).hexdigest()
OUT.write_text(
    '/* <cola-header> v1 fallback, vendored from KubeShelf /branding/v1/cola-header.css\n'
    '   (built by kubeshelf-rs web/scripts/build-plum.cjs from web/src/cola-header.css).\n'
    '   Do not edit; regenerate with vendor-header-css.py. source-sha256: ' + digest + ' */\n' + css)
print(f'Vendored {OUT.name} (source sha256 {digest}).')
