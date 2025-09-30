import asyncio
import re
import shutil
import tempfile
from pathlib import Path

import httpx
from Crypto.Cipher import AES
from pydantic import BaseModel
from tqdm import tqdm

from src.core import config, logger
from src.tool import cloudflare

log = logger.get('tangxin')
cfg = config.tx
cf_cfg = config.cloudflare


class Item(BaseModel):
    id: int
    title: str
    upper: str
    urls: list[str] | None = None
    key: bytes | None = None
    iv: bytes | None = None
    part_sizes: list[int] = []
    banner: str | None = None


class Tangxin:
    def __init__(self) -> None:
        self.client = httpx.AsyncClient(
            headers={
                'User-Agent': 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_2_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.2 Mobile/15E148 Safari/604.1',  # noqa: E501
                'Origin': cfg.host,
            },
            timeout=60,
            limits=httpx.Limits(max_keepalive_connections=10, max_connections=10),
            proxy=config.proxy if config.proxy else None,
        )

    async def get_items(self) -> list[Item]:
        results = await cloudflare.query_d1('SELECT id, title, upper FROM tx WHERE downloaded = 0 ORDER BY created_at ASC;')
        for i in results:
            i['title'] = re.sub(r'[<>:"/\\|?*]', '_', i['title'])
            i['upper'] = re.sub(r'[<>:"/\\|?*]', '_', i['upper'])
        return [Item.model_validate(i) for i in results]

    async def download(self, item: Item) -> asyncio.Task:
        dst_path = cfg.path / f'[{item.upper}]{item.title}.mp4'
        if dst_path.exists():
            log.error('File already exists %s for %s', dst_path.name, item.id)
            msg = 'File already exists'
            raise ValueError(msg)
        m3u8 = (await cloudflare.get_kv(cf_cfg.kv_id['tangxin'], item.id)).text
        key_url, iv = re.search(r'#EXT-X-KEY:METHOD=AES-128,URI="(http.+)",IV=(.+)', m3u8).groups()
        key_res = await self.client.get(key_url)
        key_res.raise_for_status()
        item.key = key_res.content
        item.iv = bytes.fromhex(iv.replace('0x', ''))
        item.urls = re.findall(r'https:.+.ts.+', m3u8)
        tmp_dir = tempfile.TemporaryDirectory(prefix='fav-tangxin-', delete=False)
        tmp_dir_path = Path(tmp_dir.name)
        with tqdm(total=0, unit='B', unit_scale=True, desc=item.title, dynamic_ncols=True) as pbar:
            tasks = [self.download_part(item, tmp_dir_path, index, pbar) for index in range(len(item.urls))]
            await asyncio.gather(*tasks)

        async def merge_task() -> None:
            log.info('Merging %s', item.banner)
            tmp_txt_path = tmp_dir_path / 'merge.txt'
            tmp_mp4_path = tmp_dir_path / 'merged.mp4'
            with tmp_txt_path.open('w') as f:
                for i in range(len(item.urls)):
                    f.write(f'file {tmp_dir_path / f"{i}.ts"}\n')
            cmd = f'ffmpeg -hide_banner -loglevel warning -f concat -safe 0 -i "{tmp_txt_path}" -c copy -y "{tmp_mp4_path}"'
            proc = await asyncio.create_subprocess_shell(cmd)
            stdout, stderr = await proc.communicate()
            log.info('Finished merge %s', item.banner)
            if proc.returncode != 0:
                msg = f'Failed to merge {item.id} {item.title}'
                raise ValueError(msg)
            if stdout:
                log.info('[stdout]\n%s', stdout.decode())
            if stderr:
                log.error('[stderr]\n%s', stderr.decode())

            shutil.move(tmp_mp4_path, dst_path)
            await cloudflare.query_d1('UPDATE tx SET downloaded = 1 WHERE id = ?;', (str(item.id),))
            tmp_dir.cleanup()
            log.notice('Finished %s', item.banner)

        return asyncio.create_task(merge_task())

    async def download_part(self, item: Item, dir_path: Path, index: int, pbar: tqdm) -> None:
        encrypt_content = b''
        async with self.client.stream('GET', item.urls[index]) as res:
            file_size = int(res.headers.get('content-length', 0))
            item.part_sizes.append(file_size)
            pbar.total = sum(item.part_sizes) / len(item.part_sizes) * len(item.urls)
            async for chunk in res.aiter_bytes():
                encrypt_content += chunk
                pbar.update(len(chunk))
        cipher = AES.new(item.key, AES.MODE_CBC, item.iv)
        content = cipher.decrypt(encrypt_content)
        (dir_path / f'{index}.ts').write_bytes(content)

    async def update(self) -> None:
        items = await self.get_items()
        if not items:
            log.info('No new content')
            return
        log.info('Found %d new content', len(items))
        merge_tasks = []
        for idx, i in enumerate(items):
            i.banner = f'[{idx + 1}/{len(items)}] {i.id} {i.title}'
            log.info('Start %s', i.banner)
            merge_task = await self.download(i)
            merge_tasks.append(merge_task)

        # Wait for all merging tasks to complete
        await asyncio.gather(*merge_tasks)
