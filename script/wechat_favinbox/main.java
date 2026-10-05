import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.RandomAccessFile;
import java.security.MessageDigest;
import java.util.Arrays;
import org.json.JSONObject;

// Saves the original of every incoming image into an inbox directory that the
// host can read (redroid bind-mounts /data, so external storage is on disk).
// Each item is <msgId>.<ext> plus <msgId>.json; the JSON is written last and
// marks the item complete. Videos and files are only recorded for now.
// Long-pressing a conversation offers a backfill of its older images.

String inboxDir = null;
int WAIT_MS = 60000;
int PAGE_SIZE = 100;
// AtomicBoolean rather than a plain boolean: BeanShell treats script variables
// captured by a lambda as final, so assigning one from the menu callback fails.
java.util.concurrent.atomic.AtomicBoolean backfillRunning = new java.util.concurrent.atomic.AtomicBoolean(false);

void onLoad() {
    File base = hostContext.getExternalFilesDir(null);
    File dir = new File(base, "FavInbox");
    dir.mkdirs();
    inboxDir = dir.getAbsolutePath();
    log("FavInbox loaded, inbox=" + inboxDir + ", host=" + hostVerName);
}

boolean alreadySaved(long msgId) {
    return new File(inboxDir, msgId + ".json").exists();
}

void onHandleMsg(Object msgInfoBean) {
    if (msgInfoBean.isSend()) return;
    if (msgInfoBean.isGroupChat() || msgInfoBean.isOfficialAccount()) return;
    if (inboxDir == null) return;

    final Object msg = msgInfoBean;
    final long msgId = msg.getMsgId();
    if (alreadySaved(msgId)) return;

    if (msg.isImage()) {
        new Thread(new Runnable() {
            public void run() {
                try {
                    saveImage(msg, msgId);
                } catch (Throwable e) {
                    log("FavInbox image " + msgId + " failed: " + e);
                    writeError(msg, msgId, "image", "error: " + e);
                }
            }
        }).start();
    } else if (msg.isVideo()) {
        writeMeta(msg, msgId, "video", null, "unsupported");
    } else if (msg.isFile()) {
        writeMeta(msg, msgId, "file", null, "unsupported");
    }
}

boolean saveImage(Object msg, long msgId) {
    Object imageMsg = msg.getImageMsg();
    File tmp = new File(inboxDir, msgId + ".part");
    tmp.delete();
    String via = null;

    if (imageMsg != null) {
        downloadImg(imageMsg, tmp.getAbsolutePath());
        if (waitStable(tmp)) via = "downloadImg";
    }
    if (via == null) {
        // Fall back to whatever WeChat itself stored locally.
        String local = null;
        try { local = msg.getImgPath(); } catch (Throwable e) {}
        if (local != null && new File(local).isFile() && new File(local).length() > 0) {
            copyFile(new File(local), tmp);
            via = "imgPath";
        }
    }
    if (via == null) {
        tmp.delete();
        writeError(msg, msgId, "image", "download failed");
        return false;
    }

    stripWeChatTrailer(tmp);
    String ext = sniffExt(tmp);
    File dest = new File(inboxDir, msgId + "." + ext);
    tmp.renameTo(dest);
    log("FavInbox saved " + dest.getName() + " (" + dest.length() + " bytes, via " + via + ")");
    writeMeta(msg, msgId, "image", dest, "ok:" + via);
    return true;
}

boolean waitStable(File f) {
    long deadline = System.currentTimeMillis() + WAIT_MS;
    long last = -1;
    int stable = 0;
    while (System.currentTimeMillis() < deadline) {
        long len = f.isFile() ? f.length() : 0;
        if (len > 0 && len == last) {
            stable++;
            if (stable >= 3) return true;
        } else {
            stable = 0;
        }
        last = len;
        Thread.sleep(500);
    }
    return f.isFile() && f.length() > 0;
}

// WeChat appends 24 bytes to downloaded images: 8 bytes of its own data and
// then a 16-byte hash. Drop them when the hash is the MD5 of the body, or when
// the body ends exactly on the image format's end marker.
void stripWeChatTrailer(File f) {
    long len = f.length();
    if (len <= 32) return;
    RandomAccessFile raf = new RandomAccessFile(f, "rw");
    try {
        byte[] head = new byte[2];
        raf.seek(0);
        raf.readFully(head);
        byte[] end = new byte[8];
        raf.seek(len - 32);
        raf.readFully(end);
        boolean jpegEnds = (head[0] & 0xff) == 0xff && (head[1] & 0xff) == 0xd8
            && (end[6] & 0xff) == 0xff && (end[7] & 0xff) == 0xd9;
        boolean pngEnds = (head[0] & 0xff) == 0x89 && end[0] == 'I' && end[1] == 'E' && end[2] == 'N' && end[3] == 'D';
        if (jpegEnds || pngEnds) {
            raf.setLength(len - 24);
            return;
        }
        byte[] tail = new byte[16];
        raf.seek(len - 16);
        raf.readFully(tail);
        MessageDigest md = MessageDigest.getInstance("MD5");
        byte[] buf = new byte[65536];
        long remaining = len - 24;
        raf.seek(0);
        while (remaining > 0) {
            int r = raf.read(buf, 0, (int) Math.min(buf.length, remaining));
            if (r <= 0) break;
            md.update(buf, 0, r);
            remaining -= r;
        }
        if (Arrays.equals(md.digest(), tail)) raf.setLength(len - 24);
    } finally {
        raf.close();
    }
}

String sniffExt(File f) {
    byte[] b = new byte[12];
    int n = 0;
    FileInputStream in = new FileInputStream(f);
    try { n = in.read(b); } finally { in.close(); }
    if (n >= 3 && (b[0] & 0xff) == 0xff && (b[1] & 0xff) == 0xd8) return "jpg";
    if (n >= 4 && (b[0] & 0xff) == 0x89 && b[1] == 'P' && b[2] == 'N' && b[3] == 'G') return "png";
    if (n >= 4 && b[0] == 'G' && b[1] == 'I' && b[2] == 'F') return "gif";
    if (n >= 12 && b[0] == 'R' && b[1] == 'I' && b[2] == 'F' && b[3] == 'F' && b[8] == 'W' && b[9] == 'E' && b[10] == 'B' && b[11] == 'P') return "webp";
    if (n >= 4 && b[0] == 'w' && b[1] == 'x' && b[2] == 'g' && b[3] == 'f') return "wxgf";
    return "bin";
}

void copyFile(File src, File dst) {
    FileInputStream in = new FileInputStream(src);
    FileOutputStream out = new FileOutputStream(dst);
    try {
        byte[] buf = new byte[65536];
        int r;
        while ((r = in.read(buf)) > 0) out.write(buf, 0, r);
    } finally {
        in.close();
        out.close();
    }
}

// Failures go to <msgId>.err so they are not picked up as finished items and a
// later backfill retries them.
void writeError(Object msg, long msgId, String type, String status) {
    writeJson(msg, msgId, type, null, status, msgId + ".err");
}

void writeMeta(Object msg, long msgId, String type, File file, String status) {
    writeJson(msg, msgId, type, file, status, msgId + ".json");
}

void writeJson(Object msg, long msgId, String type, File file, String status, String name) {
    JSONObject j = new JSONObject();
    j.put("msg_id", msgId);
    j.put("type", type);
    j.put("status", status);
    j.put("create_time", msg.getCreateTime());
    j.put("talker", msg.getTalker());
    j.put("sender", msg.getSendTalker());
    j.put("saved_at", System.currentTimeMillis());
    if (file != null) {
        j.put("file", file.getName());
        j.put("size", file.length());
    }
    if (type.equals("image") && msg.getImageMsg() != null) {
        j.put("md5", msg.getImageMsg().getMd5());
    }
    if (type.equals("file") && msg.getFileMsg() != null) {
        Object fm = msg.getFileMsg();
        j.put("title", fm.getTitle());
        j.put("ext", fm.getExt());
        j.put("declared_size", fm.getSize());
        j.put("md5", fm.getMd5());
    }
    File tmp = new File(inboxDir, name + ".part");
    FileOutputStream out = new FileOutputStream(tmp);
    try {
        out.write(j.toString().getBytes("UTF-8"));
    } finally {
        out.close();
    }
    tmp.renameTo(new File(inboxDir, name));
}

void onCreateConversationItemMenu(Object conversationBean) {
    addConversationItemMenuItem("FavInbox 补抓图片", conversation -> {
        startBackfill(conversation.getUsername());
    });
}

void startBackfill(String talker) {
    if (inboxDir == null || talker == null) return;
    if (!backfillRunning.compareAndSet(false, true)) {
        toast("补抓已在进行中");
        return;
    }
    final String t = talker;
    new Thread(new Runnable() {
        public void run() {
            int saved = 0;
            int skipped = 0;
            int failed = 0;
            try {
                toast("开始补抓图片");
                long start = 0L;
                java.util.HashSet seen = new java.util.HashSet();
                while (true) {
                    java.util.List page = queryHistoryMsg(t, start, true, PAGE_SIZE);
                    if (page == null || page.isEmpty()) break;
                    long lastTime = start;
                    for (int i = 0; i < page.size(); i++) {
                        Object m = page.get(i);
                        lastTime = Math.max(lastTime, m.getCreateTime());
                        if (m.isSend() || !m.isImage()) continue;
                        long id = m.getMsgId();
                        if (!seen.add(Long.valueOf(id))) continue;
                        if (alreadySaved(id)) {
                            skipped++;
                            continue;
                        }
                        try {
                            if (saveImage(m, id)) saved++;
                            else failed++;
                        } catch (Throwable e) {
                            failed++;
                            log("FavInbox backfill " + id + " failed: " + e);
                        }
                    }
                    if (page.size() < PAGE_SIZE || lastTime <= start) break;
                    start = lastTime;
                }
            } catch (Throwable e) {
                log("FavInbox backfill aborted: " + e);
            } finally {
                backfillRunning.set(false);
            }
            String summary = "补抓完成：新存 " + saved + "，已有 " + skipped + "，失败 " + failed;
            log("FavInbox " + summary);
            toast(summary);
        }
    }).start();
}
