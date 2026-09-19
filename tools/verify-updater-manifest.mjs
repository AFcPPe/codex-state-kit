import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

export function verifyManifest(manifest, assets, tag, repository) {
  if (manifest.version?.replace(/^v/, '') !== tag.replace(/^v/, '')) throw new Error('更新清单版本不匹配');
  const required = { 'windows-x86_64': '.exe', 'darwin-x86_64': '.app.tar.gz', 'darwin-aarch64': '.app.tar.gz' };
  for (const [platform, extension] of Object.entries(required)) {
    const entry = manifest.platforms?.[platform];
    if (!entry?.signature?.trim()) throw new Error(`${platform} 缺少签名`);
    const url = new URL(entry.url);
    const prefix = `/${repository}/releases/download/${tag}/`;
    if (url.origin !== 'https://github.com' || !url.pathname.startsWith(prefix) || url.search || url.hash) throw new Error(`${platform} 更新地址不属于当前发布`);
    const name = decodeURIComponent(url.pathname.slice(prefix.length));
    if (!name.endsWith(extension)) throw new Error(`${platform} 更新包类型错误`);
    for (const assetName of [name, `${name}.sig`]) {
      if (!assets.some((asset) => asset.name === assetName && asset.size > 0)) throw new Error(`缺少更新资源 ${assetName}`);
    }
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const [manifestFile, assetsFile, tag, repository] = process.argv.slice(2);
  verifyManifest(JSON.parse(readFileSync(manifestFile, 'utf8')), JSON.parse(readFileSync(assetsFile, 'utf8')).assets, tag, repository);
  console.log('Windows x64、macOS Intel/Apple Silicon 更新清单验证通过');
}
