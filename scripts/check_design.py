# /// script
# requires-python = ">=3.10"
# dependencies = ["playwright"]
# ///
import argparse
import sys
import tempfile
from pathlib import Path
from urllib.parse import unquote, urlparse

from playwright.sync_api import Error, sync_playwright

PAGE_TIMEOUT_MS = 15000
HOVER_WAIT_MS = 1500
DESKTOP = {"width": 1280, "height": 800}
MOBILE = {"width": 375, "height": 800}
RENDERS = {
    "desktop": {"viewport": DESKTOP, "reduced_motion": "reduce"},
    "mobile": {"viewport": MOBILE, "reduced_motion": "reduce"},
    "dark": {"viewport": DESKTOP, "reduced_motion": "reduce", "color_scheme": "dark"},
}
WAIT_READY = """async () => {
  if (window.MathJax && window.MathJax.startup) await window.MathJax.startup.promise;
  await document.fonts.ready;
  return true;
}"""
LINK_VALUES = """() => [...document.querySelectorAll("[href], [src]")]
  .map(el => el.getAttribute("href") ?? el.getAttribute("src"))"""
OVERFLOW = "() => document.documentElement.scrollWidth > document.documentElement.clientWidth"


def open_page(context, html, errors):
    page = context.new_page()
    page.set_default_timeout(PAGE_TIMEOUT_MS)
    page.on("console", lambda msg: msg.type == "error" and errors.append(f"console: {msg.text}"))
    page.on("pageerror", lambda exc: errors.append(f"pageerror: {exc}"))
    page.on("requestfailed", lambda req: errors.append(f"requestfailed: {req.url} {req.failure}"))
    page.on("response", lambda res: res.status >= 400 and errors.append(f"http {res.status}: {res.url}"))
    page.goto(html.as_uri(), wait_until="load")
    page.wait_for_function(WAIT_READY)
    return page


def broken_links(page, html):
    broken = []
    for value in page.evaluate(LINK_VALUES):
        parsed = urlparse(value)
        if parsed.scheme or parsed.netloc or value.startswith(("#", "/")) or not parsed.path:
            continue
        if not (html.parent / unquote(parsed.path)).exists():
            broken.append(f"broken link: {value}")
    return broken


def check(browser, root, html, out):
    rel = html.relative_to(root).as_posix()
    stem = rel.removesuffix(".html").replace("/", "__")
    errors = []
    for name, context_options in RENDERS.items():
        found = []
        context = browser.new_context(**context_options)
        try:
            page = open_page(context, html, found)
            page.screenshot(path=out / f"{stem}.{name}.png", full_page=True)
            if page.locator("mjx-merror").count():
                found.append("MathJax mjx-merror")
            if name == "mobile" and page.evaluate(OVERFLOW):
                found.append("horizontal overflow")
            if name == "desktop":
                found += broken_links(page, html)
        except Error as exc:
            found.append(f"{type(exc).__name__}: {exc.message.splitlines()[0]}")
        finally:
            errors_before_close = list(found)
            context.close()
        errors += [f"[{name}] {e}" for e in errors_before_close]
    if rel == "index.html":
        found = []
        context = browser.new_context(viewport=DESKTOP)
        try:
            page = open_page(context, html, found)
            for i, card in enumerate(page.locator(".card").all()):
                card.hover()
                page.wait_for_timeout(HOVER_WAIT_MS)
                card.screenshot(path=out / f"{stem}.hover-{i}.png")
        except Error as exc:
            found.append(f"{type(exc).__name__}: {exc.message.splitlines()[0]}")
        finally:
            errors_before_close = list(found)
            context.close()
        errors += [f"[hover] {e}" for e in errors_before_close]
    return rel, errors


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("root", nargs="?", default="docs/design", type=Path)
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()
    root = args.root.resolve()
    out = args.out or Path(tempfile.mkdtemp())
    out.mkdir(parents=True, exist_ok=True)
    failed = False
    with sync_playwright() as p:
        browser = p.chromium.launch(channel="chrome")
        for html in sorted(root.rglob("*.html")):
            rel, errors = check(browser, root, html, out)
            print(f"{'FAIL' if errors else 'PASS'} {rel}")
            for error in errors:
                print(f"    {error}")
            failed = failed or bool(errors)
        browser.close()
    print(out.resolve())
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
