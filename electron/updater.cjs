//! Installer download + verification for the in-app updater.
//!
//! Kept free of Electron so it can be exercised with plain Node: the GitHub
//! release asset URL redirects to a CDN, and the API publishes a sha256
//! `digest` that must match before the file is offered for install.

const crypto = require('crypto');
const fs = require('fs');
const http = require('http');
const https = require('https');
const path = require('path');

/// Follow GitHub's asset redirects and resolve with the final response.
function followRedirects(url, redirects = 0) {
  return new Promise((resolve, reject) => {
    const client = url.startsWith('http:') ? http : https;
    const request = client.get(
      url,
      {
        headers: {
          'User-Agent': 'O11A1-Updater',
          Accept: 'application/octet-stream',
        },
      },
      (response) => {
        const status = response.statusCode || 0;
        if (
          [301, 302, 303, 307, 308].includes(status) &&
          response.headers.location &&
          redirects < 6
        ) {
          response.resume();
          followRedirects(new URL(response.headers.location, url).toString(), redirects + 1).then(
            resolve,
            reject,
          );
          return;
        }
        resolve(response);
      },
    );
    request.setTimeout(120000, () => request.destroy(new Error('更新下载超时')));
    request.on('error', reject);
  });
}

/// Download one release asset to `target` atomically. `onProgress(received,
/// total)` is throttled by the caller. Resolves with the file size.
async function downloadAsset(asset, target, onProgress) {
  const part = `${target}.part`;
  fs.mkdirSync(path.dirname(target), { recursive: true });
  await fs.promises.rm(part, { force: true }).catch(() => {});

  const response = await followRedirects(asset.url);
  if (response.statusCode !== 200) {
    response.resume();
    throw new Error(`下载失败（HTTP ${response.statusCode}）`);
  }
  const total = Number(response.headers['content-length']) || asset.size || 0;
  const digest = String(asset.digest || '')
    .replace(/^sha256:/i, '')
    .toLowerCase();
  const hash = digest ? crypto.createHash('sha256') : null;
  const file = fs.createWriteStream(part);
  let received = 0;
  let lastReport = 0;

  response.on('data', (chunk) => {
    received += chunk.length;
    if (hash) hash.update(chunk);
    const now = Date.now();
    if (onProgress && now - lastReport >= 250) {
      lastReport = now;
      onProgress(received, total);
    }
  });

  try {
    await new Promise((resolve, reject) => {
      response.on('error', reject);
      file.on('error', reject);
      file.on('finish', resolve);
      response.pipe(file);
    });
  } catch (error) {
    file.destroy();
    await fs.promises.rm(part, { force: true }).catch(() => {});
    throw error;
  }

  await new Promise((resolve) => file.close(resolve));
  if (asset.size && received !== asset.size) {
    await fs.promises.rm(part, { force: true }).catch(() => {});
    throw new Error(`安装包大小不匹配（${received}/${asset.size}）`);
  }
  if (hash && hash.digest('hex') !== digest) {
    await fs.promises.rm(part, { force: true }).catch(() => {});
    throw new Error('安装包校验失败');
  }
  await fs.promises.rename(part, target);
  if (onProgress) onProgress(received, received);
  return received;
}

/// PowerShell that waits for the given PIDs to exit, runs the silent NSIS
/// installer and relaunches the installed app. Exposed for the updater test:
/// the quoting has to survive paths with spaces and apostrophes.
function installerScript(installer, relaunch, pids) {
  const quote = (value) => `'${String(value).replace(/'/g, "''")}'`;
  const targets = (pids || []).filter((pid) => Number(pid) > 0);
  return [
    "$ErrorActionPreference = 'SilentlyContinue'",
    `$targets = @(${targets.join(', ')})`,
    'for ($i = 0; $i -lt 200; $i++) {',
    '  $alive = $false',
    '  foreach ($procId in $targets) {',
    '    if (Get-Process -Id $procId -ErrorAction SilentlyContinue) { $alive = $true }',
    '  }',
    '  if (-not $alive) { break }',
    '  Start-Sleep -Milliseconds 200',
    '}',
    'Start-Sleep -Milliseconds 500',
    `$installer = ${quote(installer)}`,
    "Start-Process -FilePath $installer -ArgumentList '/S','--updated' -Wait",
    `if (Test-Path ${quote(relaunch)}) { Start-Process -FilePath ${quote(relaunch)} }`,
  ].join('\n');
}

function encodePowerShell(script) {
  return Buffer.from(script, 'utf16le').toString('base64');
}

module.exports = { followRedirects, downloadAsset, installerScript, encodePowerShell };
