import { createReadStream, statSync } from 'node:fs';
import { stat } from 'node:fs/promises';
import { createServer } from 'node:http';
import { extname, join, resolve } from 'node:path';

const root = resolve('out');
const host = process.env.HOST ?? '127.0.0.1';
const port = Number(process.env.PORT ?? 3000);

const contentTypes = new Map([
  ['.css', 'text/css; charset=utf-8'],
  ['.html', 'text/html; charset=utf-8'],
  ['.ico', 'image/x-icon'],
  ['.jpg', 'image/jpeg'],
  ['.js', 'text/javascript; charset=utf-8'],
  ['.json', 'application/json; charset=utf-8'],
  ['.png', 'image/png'],
  ['.svg', 'image/svg+xml'],
  ['.txt', 'text/plain; charset=utf-8'],
  ['', 'application/json; charset=utf-8'],
]);

try {
  const output = statSync(root);
  if (!output.isDirectory()) {
    throw new Error('out is not a directory');
  }
} catch {
  console.error('Run `npm run build` before `npm run preview`.');
  process.exit(1);
}

function contentType(file) {
  return contentTypes.get(extname(file)) ?? 'application/octet-stream';
}

async function resolveFile(urlPath) {
  const pathname = urlPath === '/' ? '/index.html' : decodeURIComponent(urlPath);
  const candidate = resolve(join(root, pathname));

  if (!candidate.startsWith(root)) {
    return undefined;
  }

  try {
    const exact = await stat(candidate);
    if (exact.isFile()) {
      return candidate;
    }
  } catch {
    // Continue to the clean-URL fallback.
  }

  if (extname(candidate)) {
    return undefined;
  }

  const htmlCandidate = `${candidate}.html`;

  try {
    const html = await stat(htmlCandidate);
    return html.isFile() ? htmlCandidate : undefined;
  } catch {
    return undefined;
  }
}

const server = createServer(async (request, response) => {
  const url = new URL(request.url ?? '/', `http://${host}:${port}`);
  const file = await resolveFile(url.pathname);

  if (!file) {
    response.writeHead(404, { 'Content-Type': 'text/plain; charset=utf-8' });
    response.end('Not found');
    return;
  }

  response.writeHead(200, { 'Content-Type': contentType(file) });
  createReadStream(file).pipe(response);
});

server.listen(port, host, () => {
  console.log(`Hekma docs preview: http://${host}:${port}`);
});
