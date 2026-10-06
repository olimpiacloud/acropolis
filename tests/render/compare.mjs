import { chromium } from "playwright-core";
import http from "node:http";
import fs from "node:fs";
import path from "node:path";

const [, , dirA, dirB, outDir] = process.argv;
const exe = fs.readdirSync(process.env.HOME + "/.cache/ms-playwright").filter((d) => d.startsWith("chromium-")).map((d) => process.env.HOME + "/.cache/ms-playwright/" + d + "/chrome-linux64/chrome").find((p) => fs.existsSync(p));
const types = { ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".svg": "image/svg+xml", ".png": "image/png" };

function serve(root) {
  return new Promise((resolve) => {
    const s = http.createServer((req, res) => {
      let p = path.join(root, decodeURIComponent(req.url.split("?")[0]));
      if (!fs.existsSync(p) || fs.statSync(p).isDirectory()) p = path.join(root, "index.html");
      res.writeHead(200, { "content-type": types[path.extname(p)] || "application/octet-stream" });
      fs.createReadStream(p).pipe(res);
    });
    s.listen(0, "127.0.0.1", () => resolve(s));
  });
}

async function render(browser, root, name) {
  const server = await serve(root);
  const page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
  const errors = [];
  page.on("console", (m) => { if (m.type() === "error") errors.push(m.text()); });
  page.on("pageerror", (e) => errors.push(String(e)));
  await page.goto(`http://127.0.0.1:${server.address().port}/`, { waitUntil: "networkidle" });
  await page.waitForTimeout(300);
  const clicks = await page.$$("button");
  if (clicks.length) await clicks[0].click();
  await page.waitForTimeout(100);
  const html = (await page.evaluate(() => document.body.innerHTML)).replace(/-[A-Za-z0-9_]{8}\.(svg|png|jpg|jpeg|gif|webp|woff2?|css|js)/g, "-HASH.$1").replace(/>\s+</g, "><").trim();
  const shot = await page.screenshot({ fullPage: true, animations: "disabled" });
  fs.writeFileSync(path.join(outDir, name + ".png"), shot);
  server.close();
  return { html, shot, errors };
}

const browser = await chromium.launch({ executablePath: exe });
fs.mkdirSync(outDir, { recursive: true });
const a = await render(browser, dirA, "a");
const b = await render(browser, dirB, "b");
await browser.close();
if (a.html !== b.html) { console.error("A:", a.html); console.error("B:", b.html); }
const result = { sameDom: a.html === b.html, sameScreenshot: Buffer.compare(a.shot, b.shot) === 0, errorsA: a.errors, errorsB: b.errors, domLength: [a.html.length, b.html.length] };
console.log(JSON.stringify(result));
process.exit(result.sameDom && result.sameScreenshot && !a.errors.length && !b.errors.length ? 0 : 1);
