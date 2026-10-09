import java.io.BufferedReader;
import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.io.RandomAccessFile;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.net.URLDecoder;
import java.security.MessageDigest;
import java.security.SecureRandom;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

// FavInbox has two jobs.
//
// 1. Inbox: it saves the original of every incoming private-chat image into a
//    directory the host can read (redroid bind-mounts /data, so external storage
//    is on disk). Each item is <msgId>.<ext> plus <msgId>.json; the JSON is
//    written last and marks the item complete. Long-pressing a conversation
//    offers a backfill of its older images. The fav `wechat` source drains this
//    through the host's favinbox_server.py.
//
// 2. Bridge: an HTTP API inside the WeChat process that exposes WAuxiliary's
//    capabilities (message events, history, original images, contacts, groups,
//    sending, Moments) so fav decides what to do with them. It listens on
//    BRIDGE_PORT inside the container and requires the token the plugin keeps in
//    FavBridge/token; the host's favinbox_server.py reads that file and forwards
//    /wx/* to it, so fav only ever talks to the host service. See README.md.
//
// The script runs in WAuxiliary's BeanShell, which is why it avoids generics and
// `var`, never returns from inside a try that has a finally (BeanShell then
// returns void), and keeps state shared with callbacks in Atomic* holders: script
// variables captured by a lambda or an anonymous class are treated as final.

String PLUGIN_VERSION = "0.3.0";
int WAIT_MS = 60000;
int PAGE_SIZE = 100;
int MAX_JSON_BODY = 1024 * 1024;
long MAX_UPLOAD = 200L * 1024 * 1024;
long EVENTS_ROTATE_BYTES = 16L * 1024 * 1024;
long UPLOAD_TTL_MS = 24L * 3600 * 1000;
int SEND_CALLBACK_MS = 15000;

String inboxDir = null;
String bridgeDir = null;
String uploadDir = null;
String bridgeToken = null;
long startedAt = System.currentTimeMillis();
java.util.concurrent.atomic.AtomicBoolean backfillRunning = new java.util.concurrent.atomic.AtomicBoolean(false);
java.util.concurrent.atomic.AtomicLong lastSeq = new java.util.concurrent.atomic.AtomicLong(0);
java.util.concurrent.atomic.AtomicLong firstSeqInCurrent = new java.util.concurrent.atomic.AtomicLong(0);
java.util.concurrent.locks.ReentrantLock eventLock = new java.util.concurrent.locks.ReentrantLock();
java.util.concurrent.atomic.AtomicReference serverRef = new java.util.concurrent.atomic.AtomicReference();
java.util.concurrent.ExecutorService pool = null;

void onLoad() {
    File base = hostContext.getExternalFilesDir(null);
    File dir = new File(base, "FavInbox");
    dir.mkdirs();
    inboxDir = dir.getAbsolutePath();
    File bdir = new File(base, "FavBridge");
    bdir.mkdirs();
    bridgeDir = bdir.getAbsolutePath();
    File udir = new File(cacheDir, "bridge-uploads");
    udir.mkdirs();
    uploadDir = udir.getAbsolutePath();
    bridgeToken = loadOrCreateToken();
    recoverSeq();
    log("FavInbox " + PLUGIN_VERSION + " loaded, inbox=" + inboxDir + ", bridge=" + bridgeDir + ", host=" + hostVerName);
    if (getBoolean("bridge_enabled", true)) {
        try {
            startBridge(getInt("bridge_port", 9971));
        } catch (Throwable e) {
            log("FavBridge failed to start: " + e);
        }
    }
}

void onUnload() {
    stopBridge();
}

// ---------------------------------------------------------------------------
// Inbox
// ---------------------------------------------------------------------------

boolean alreadySaved(long msgId) {
    return new File(inboxDir, msgId + ".json").exists();
}

void onHandleMsg(Object msgInfoBean) {
    try {
        recordEvent("message", messageJson(msgInfoBean));
    } catch (Throwable e) {
        log("FavBridge event failed: " + e);
    }
    if (!getBoolean("inbox_enabled", true)) return;
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
    File tmp = new File(inboxDir, msgId + ".part");
    String[] via = new String[1];
    File done = fetchOriginal(msg, tmp, via);
    if (done == null) {
        writeError(msg, msgId, "image", "download failed");
        return false;
    }
    File dest = new File(inboxDir, done.getName());
    done.renameTo(dest);
    log("FavInbox saved " + dest.getName() + " (" + dest.length() + " bytes, via " + via[0] + ")");
    writeMeta(msg, msgId, "image", dest, "ok:" + via[0]);
    return true;
}

// Downloads the original of an image message into `tmp`, strips WeChat's
// trailer and renames it to <msgId>.<ext> next to `tmp`. Returns that file, or
// null when neither the CDN download nor WeChat's local copy produced bytes.
File fetchOriginal(Object msg, File tmp, String[] via) {
    Object imageMsg = msg.getImageMsg();
    tmp.delete();
    via[0] = null;
    if (imageMsg != null) {
        downloadImg(imageMsg, tmp.getAbsolutePath());
        if (waitStable(tmp)) via[0] = "downloadImg";
    }
    if (via[0] == null) {
        // Fall back to whatever WeChat itself stored locally.
        String local = null;
        try { local = msg.getImgPath(); } catch (Throwable e) {}
        if (local != null && new File(local).isFile() && new File(local).length() > 0) {
            copyFile(new File(local), tmp);
            via[0] = "imgPath";
        }
    }
    if (via[0] == null) {
        tmp.delete();
        return null;
    }
    return finishImage(tmp, String.valueOf(msg.getMsgId()));
}

File finishImage(File tmp, String name) {
    stripWeChatTrailer(tmp);
    File dest = new File(tmp.getParentFile(), name + "." + sniffExt(tmp));
    dest.delete();
    tmp.renameTo(dest);
    return dest;
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
    writeFileAtomic(new File(inboxDir, name), j.toString().getBytes("UTF-8"));
}

void writeFileAtomic(File dest, byte[] data) {
    File tmp = new File(dest.getParentFile(), dest.getName() + ".part");
    FileOutputStream out = new FileOutputStream(tmp);
    try {
        out.write(data);
    } finally {
        out.close();
    }
    tmp.renameTo(dest);
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

// ---------------------------------------------------------------------------
// Bridge: events
// ---------------------------------------------------------------------------

void onNewFriend(String wxid, String ticket, int scene, JSONObject info) {
    JSONObject j = new JSONObject();
    j.put("wxid", wxid);
    j.put("ticket", ticket);
    j.put("scene", scene);
    j.put("info", info);
    recordEvent("new_friend", j);
}

void onMemberChange(String type, String groupWxid, String userWxid, String userName) {
    JSONObject j = new JSONObject();
    j.put("change", type);
    j.put("room", groupWxid);
    j.put("wxid", userWxid);
    j.put("name", userName);
    recordEvent("member_change", j);
}

void onRecvPayMsg(Object payMsgBean) {
    recordEvent("pay", beanJson(payMsgBean, 0));
}

// Events are appended to FavBridge/events.jsonl, one JSON object per line that
// always starts with {"seq":N, so a reader can find a sequence number without
// parsing the line. The file rolls over to events.1.jsonl at EVENTS_ROTATE_BYTES.
File eventsFile() { return new File(bridgeDir, "events.jsonl"); }
File oldEventsFile() { return new File(bridgeDir, "events.1.jsonl"); }

void recordEvent(String kind, JSONObject data) {
    if (bridgeDir == null) return;
    eventLock.lock();
    try {
        File f = eventsFile();
        if (f.length() > EVENTS_ROTATE_BYTES) {
            File old = oldEventsFile();
            old.delete();
            f.renameTo(old);
            firstSeqInCurrent.set(0);
        }
        long seq = lastSeq.incrementAndGet();
        if (firstSeqInCurrent.get() == 0) firstSeqInCurrent.set(seq);
        JSONObject rest = new JSONObject();
        rest.put("kind", kind);
        rest.put("at", System.currentTimeMillis());
        rest.put("data", data);
        String line = "{\"seq\":" + seq + "," + rest.toString().substring(1) + "\n";
        FileOutputStream out = new FileOutputStream(f, true);
        try {
            out.write(line.getBytes("UTF-8"));
        } finally {
            out.close();
        }
    } finally {
        eventLock.unlock();
    }
}

long lineSeq(String line) {
    if (!line.startsWith("{\"seq\":")) return -1;
    int end = line.indexOf(',', 7);
    if (end < 0) return -1;
    try {
        return Long.parseLong(line.substring(7, end));
    } catch (Throwable e) {
        return -1;
    }
}

String lastLine(File f) {
    if (!f.isFile() || f.length() == 0) return null;
    String s = null;
    RandomAccessFile raf = new RandomAccessFile(f, "r");
    try {
        long len = raf.length();
        int n = (int) Math.min(len, 65536L);
        byte[] buf = new byte[n];
        raf.seek(len - n);
        raf.readFully(buf);
        s = new String(buf, "UTF-8").trim();
    } finally {
        raf.close();
    }
    int nl = s.lastIndexOf('\n');
    return nl >= 0 ? s.substring(nl + 1) : s;
}

String firstLine(File f) {
    if (!f.isFile() || f.length() == 0) return null;
    String line = null;
    BufferedReader r = new BufferedReader(new InputStreamReader(new FileInputStream(f), "UTF-8"));
    try {
        line = r.readLine();
    } finally {
        r.close();
    }
    return line;
}

void recoverSeq() {
    long seq = 0;
    String last = lastLine(eventsFile());
    if (last == null) last = lastLine(oldEventsFile());
    if (last != null) seq = Math.max(0, lineSeq(last));
    lastSeq.set(seq);
    String first = firstLine(eventsFile());
    firstSeqInCurrent.set(first == null ? 0 : Math.max(0, lineSeq(first)));
}

// Up to `limit` raw event lines with seq > after, oldest first. Waits up to
// waitMs for the first new event, so a caller can long-poll.
List readEvents(long after, int limit, long waitMs) {
    long deadline = System.currentTimeMillis() + waitMs;
    while (lastSeq.get() <= after && System.currentTimeMillis() < deadline) {
        Thread.sleep(250);
    }
    List out = new ArrayList();
    if (lastSeq.get() <= after) return out;
    long first = firstSeqInCurrent.get();
    if (first == 0 || after + 1 < first) collectEvents(oldEventsFile(), after, limit, out);
    if (out.size() < limit) collectEvents(eventsFile(), after, limit, out);
    return out;
}

void collectEvents(File f, long after, int limit, List out) {
    if (!f.isFile()) return;
    BufferedReader r = new BufferedReader(new InputStreamReader(new FileInputStream(f), "UTF-8"));
    try {
        String line;
        while ((line = r.readLine()) != null && out.size() < limit) {
            if (lineSeq(line) > after) out.add(line);
        }
    } finally {
        r.close();
    }
}

// ---------------------------------------------------------------------------
// Bridge: JSON views of WAuxiliary objects
// ---------------------------------------------------------------------------

Object nn(Object v) {
    return v == null ? JSONObject.NULL : v;
}

Object getter(Object o, String name) {
    if (o == null) return null;
    try {
        return o.getClass().getMethod(name, new Class[0]).invoke(o, new Object[0]);
    } catch (Throwable e) {
        return null;
    }
}

boolean isTrue(Object o, String name) {
    Object v = getter(o, name);
    return v != null && v.equals(Boolean.TRUE);
}

boolean isSimple(Object v) {
    return v instanceof String || v instanceof Number || v instanceof Boolean || v instanceof Character;
}

String snake(String s) {
    StringBuilder b = new StringBuilder();
    for (int i = 0; i < s.length(); i++) {
        char c = s.charAt(i);
        if (Character.isUpperCase(c)) {
            if (i > 0) b.append('_');
            b.append(Character.toLowerCase(c));
        } else {
            b.append(c);
        }
    }
    return b.toString();
}

Object jsonValue(Object v, int depth) {
    if (v == null) return JSONObject.NULL;
    if (isSimple(v)) return v;
    if (v instanceof JSONObject || v instanceof JSONArray) return v;
    if (v instanceof java.util.Collection) {
        JSONArray a = new JSONArray();
        java.util.Iterator it = ((java.util.Collection) v).iterator();
        while (it.hasNext()) a.put(jsonValue(it.next(), depth));
        return a;
    }
    if (v instanceof Map) {
        JSONObject j = new JSONObject();
        java.util.Iterator it = ((Map) v).entrySet().iterator();
        while (it.hasNext()) {
            java.util.Map.Entry e = (java.util.Map.Entry) it.next();
            j.put(String.valueOf(e.getKey()), jsonValue(e.getValue(), depth));
        }
        return j;
    }
    if (depth <= 0) return String.valueOf(v);
    return beanJson(v, depth - 1);
}

// Every public no-argument getX()/isX() of a bean, keyed in snake_case. Used for
// the structures WAuxiliary does not document field by field (FriendInfo,
// GroupInfo, ContactLabelBean) and for the nested message structures.
JSONObject beanJson(Object o, int depth) {
    JSONObject j = new JSONObject();
    if (o == null) return j;
    java.lang.reflect.Method[] methods = o.getClass().getMethods();
    for (int i = 0; i < methods.length; i++) {
        java.lang.reflect.Method m = methods[i];
        if (m.getParameterTypes().length != 0) continue;
        if (m.getDeclaringClass() == Object.class) continue;
        String name = m.getName();
        String key = null;
        if (name.startsWith("get") && name.length() > 3) key = name.substring(3);
        else if (name.startsWith("is") && name.length() > 2) key = name.substring(2);
        if (key == null) continue;
        try {
            j.put(snake(key), jsonValue(m.invoke(o, new Object[0]), depth));
        } catch (Throwable e) {
            // A getter that throws is left out.
        }
    }
    return j;
}

String[] KIND_CHECKS = new String[] {
    "isRecalled", "recalled", "isSystem", "system", "isPat", "pat", "isQuote", "quote",
    "isImage", "image", "isVideo", "video", "isVoice", "voice", "isEmoji", "emoji",
    "isLocation", "location", "isShareCard", "share_card", "isLink", "link", "isFile", "file",
    "isVideoNumberVideo", "channel_video", "isNote", "note", "isTransfer", "transfer",
    "isRedBag", "red_bag", "isVoipVideo", "voip_video", "isVoipVoice", "voip_voice",
    "isVoip", "voip", "isApp", "app", "isText", "text"
};

String messageKind(Object msg) {
    for (int i = 0; i < KIND_CHECKS.length; i += 2) {
        if (isTrue(msg, KIND_CHECKS[i])) return KIND_CHECKS[i + 1];
    }
    return "other";
}

String chatKind(Object msg) {
    if (isTrue(msg, "isOfficialAccount")) return "official";
    if (isTrue(msg, "isImChatroom")) return "im_group";
    if (isTrue(msg, "isGroupChat") || isTrue(msg, "isChatroom")) return "group";
    if (isTrue(msg, "isOpenIM")) return "im_private";
    return "private";
}

JSONObject messageJson(Object msg) {
    JSONObject j = new JSONObject();
    j.put("msg_id", nn(getter(msg, "getMsgId")));
    j.put("type", nn(getter(msg, "getType")));
    j.put("kind", messageKind(msg));
    j.put("chat", chatKind(msg));
    j.put("create_time", nn(getter(msg, "getCreateTime")));
    j.put("talker", nn(getter(msg, "getTalker")));
    j.put("sender", nn(getter(msg, "getSendTalker")));
    j.put("is_send", isTrue(msg, "isSend"));
    j.put("is_at_me", isTrue(msg, "isAtMe"));
    j.put("is_notify_all", isTrue(msg, "isNotifyAll"));
    j.put("is_announce_all", isTrue(msg, "isAnnounceAll"));
    j.put("at_users", jsonValue(getter(msg, "getAtUserList"), 0));
    j.put("content", nn(getter(msg, "getContent")));
    j.put("msg_source", nn(getter(msg, "getMsgSource")));
    String[] nested = new String[] {"getImageMsg", "image", "getFileMsg", "file", "getQuoteMsg", "quote", "getPatMsg", "pat"};
    for (int i = 0; i < nested.length; i += 2) {
        Object v = getter(msg, nested[i]);
        if (v != null) j.put(nested[i + 1], beanJson(v, 0));
    }
    return j;
}

JSONArray messagesJson(List msgs) {
    JSONArray a = new JSONArray();
    if (msgs == null) return a;
    for (int i = 0; i < msgs.size(); i++) a.put(messageJson(msgs.get(i)));
    return a;
}

JSONArray beansJson(List items) {
    JSONArray a = new JSONArray();
    if (items == null) return a;
    for (int i = 0; i < items.size(); i++) {
        Object v = items.get(i);
        a.put(isSimple(v) ? v : beanJson(v, 0));
    }
    return a;
}

// ---------------------------------------------------------------------------
// Bridge: HTTP server
// ---------------------------------------------------------------------------

String loadOrCreateToken() {
    File f = new File(bridgeDir, "token");
    String existing = firstLine(f);
    if (existing != null && existing.trim().length() >= 32) return existing.trim();
    byte[] raw = new byte[32];
    new SecureRandom().nextBytes(raw);
    StringBuilder b = new StringBuilder();
    for (int i = 0; i < raw.length; i++) b.append(Integer.toHexString((raw[i] & 0xff) | 0x100).substring(1));
    writeFileAtomic(f, (b.toString() + "\n").getBytes("UTF-8"));
    log("FavBridge created a new token in " + f);
    return b.toString();
}

void startBridge(int port) {
    stopBridge();
    // A reload that skipped onUnload leaves the old socket bound; it is parked in
    // the system properties so this instance can close it.
    Object stale = System.getProperties().get("favbridge.server");
    if (stale != null) {
        try { ((ServerSocket) stale).close(); } catch (Throwable e) {}
    }
    final ServerSocket ss = new ServerSocket();
    ss.setReuseAddress(true);
    ss.bind(new InetSocketAddress("0.0.0.0", port));
    System.getProperties().put("favbridge.server", ss);
    serverRef.set(ss);
    pool = java.util.concurrent.Executors.newFixedThreadPool(8);
    final java.util.concurrent.ExecutorService workers = pool;
    Thread t = new Thread(new Runnable() {
        public void run() {
            while (!ss.isClosed()) {
                try {
                    final Socket s = ss.accept();
                    workers.execute(new Runnable() {
                        public void run() {
                            serve(s);
                        }
                    });
                } catch (Throwable e) {
                    if (!ss.isClosed()) log("FavBridge accept failed: " + e);
                }
            }
        }
    });
    t.setName("FavBridge");
    t.setDaemon(true);
    t.start();
    log("FavBridge listening on " + port);
}

void stopBridge() {
    Object ss = serverRef.getAndSet(null);
    if (ss != null) {
        try { ((ServerSocket) ss).close(); } catch (Throwable e) {}
        System.getProperties().remove("favbridge.server");
    }
    if (pool != null) {
        pool.shutdownNow();
        pool = null;
    }
}

// One request per connection: parse, dispatch, answer, close.
void serve(Socket s) {
    try {
        s.setSoTimeout(120000);
        InputStream in = new java.io.BufferedInputStream(s.getInputStream());
        OutputStream out = new java.io.BufferedOutputStream(s.getOutputStream());
        Map req = null;
        try {
            req = readRequest(in);
        } catch (Throwable e) {
            sendJson(out, 400, error("bad request: " + e.getMessage()));
            return;
        }
        if (req == null) return;
        Object result;
        int status = 200;
        try {
            result = guarded(req, in);
        } catch (Throwable e) {
            Throwable cause = rootCause(e);
            status = statusFor(cause);
            result = error(String.valueOf(cause.getMessage() == null ? cause.toString() : cause.getMessage()));
            if (status == 500) log("FavBridge " + req.get("method") + " " + req.get("path") + " failed: " + cause);
        }
        if (result instanceof File) {
            sendFile(out, (File) result);
            ((File) result).delete();
        } else if (result instanceof JSONArray) {
            sendBytes(out, status, "application/json; charset=utf-8", result.toString().getBytes("UTF-8"));
        } else if (result instanceof String) {
            sendBytes(out, status, "application/json; charset=utf-8", ((String) result).getBytes("UTF-8"));
        } else {
            sendJson(out, status, (JSONObject) result);
        }
    } catch (Throwable e) {
        log("FavBridge connection failed: " + e);
    } finally {
        try { s.close(); } catch (Throwable e) {}
    }
}

// Runs a route behind a Java-level Callable. A WAuxiliary function this host
// version lacks fails as a BeanShell EvalError, which a script catch does not
// see; through the Callable it arrives as an ExecutionException instead of
// killing the worker thread.
Object guarded(Map req, InputStream in) {
    final Map r = req;
    final InputStream input = in;
    java.util.concurrent.FutureTask task = new java.util.concurrent.FutureTask(new java.util.concurrent.Callable() {
        public Object call() {
            return route(r, input);
        }
    });
    task.run();
    return task.get();
}

Throwable rootCause(Throwable e) {
    Throwable c = e;
    for (int i = 0; i < 8; i++) {
        Throwable next = null;
        if (c instanceof java.lang.reflect.InvocationTargetException) next = ((java.lang.reflect.InvocationTargetException) c).getTargetException();
        else if (c instanceof java.util.concurrent.ExecutionException) next = c.getCause();
        else if (c instanceof java.lang.reflect.UndeclaredThrowableException) next = c.getCause();
        else if (c.getClass().getName().equals("bsh.TargetError")) next = (Throwable) getter(c, "getTarget");
        if (next == null || next == c) break;
        c = next;
    }
    return c;
}

// Exceptions double as HTTP statuses: SecurityException 401, FileNotFoundException
// 404, IllegalArgumentException/JSONException 400, anything else 500.
int statusFor(Throwable e) {
    if (e instanceof SecurityException) return 401;
    if (e instanceof java.io.FileNotFoundException) return 404;
    if (e instanceof IllegalArgumentException || e instanceof JSONException) return 400;
    return 500;
}

JSONObject error(String message) {
    JSONObject j = new JSONObject();
    j.put("ok", false);
    j.put("error", message);
    return j;
}

JSONObject ok() {
    JSONObject j = new JSONObject();
    j.put("ok", true);
    return j;
}

Map readRequest(InputStream in) {
    ByteArrayOutputStream head = new ByteArrayOutputStream();
    int matched = 0;
    while (matched < 4) {
        int c = in.read();
        if (c < 0) {
            if (head.size() == 0) return null;
            throw new IllegalArgumentException("truncated headers");
        }
        head.write(c);
        if (head.size() > 32768) throw new IllegalArgumentException("headers too large");
        if ((matched % 2 == 0 && c == '\r') || (matched % 2 == 1 && c == '\n')) matched++;
        else matched = c == '\r' ? 1 : 0;
    }
    String[] lines = new String(head.toByteArray(), "ISO-8859-1").split("\r\n");
    String[] first = lines[0].split(" ");
    if (first.length < 2) throw new IllegalArgumentException("bad request line");
    Map headers = new HashMap();
    for (int i = 1; i < lines.length; i++) {
        int colon = lines[i].indexOf(':');
        if (colon > 0) headers.put(lines[i].substring(0, colon).trim().toLowerCase(), lines[i].substring(colon + 1).trim());
    }
    String target = first[1];
    int q = target.indexOf('?');
    Map query = new HashMap();
    if (q >= 0) {
        String[] pairs = target.substring(q + 1).split("&");
        for (int i = 0; i < pairs.length; i++) {
            if (pairs[i].length() == 0) continue;
            int eq = pairs[i].indexOf('=');
            String k = eq >= 0 ? pairs[i].substring(0, eq) : pairs[i];
            String v = eq >= 0 ? pairs[i].substring(eq + 1) : "";
            query.put(URLDecoder.decode(k, "UTF-8"), URLDecoder.decode(v, "UTF-8"));
        }
    }
    Map req = new HashMap();
    req.put("method", first[0].toUpperCase());
    req.put("path", URLDecoder.decode(q >= 0 ? target.substring(0, q) : target, "UTF-8"));
    req.put("query", query);
    req.put("headers", headers);
    String cl = (String) headers.get("content-length");
    req.put("length", Long.valueOf(cl == null ? 0L : Long.parseLong(cl)));
    return req;
}

byte[] readBody(Map req, InputStream in, long max) {
    long len = ((Long) req.get("length")).longValue();
    if (len > max) throw new IllegalArgumentException("body too large");
    byte[] buf = new byte[(int) len];
    int off = 0;
    while (off < len) {
        int r = in.read(buf, off, (int) len - off);
        if (r < 0) throw new IllegalArgumentException("truncated body");
        off += r;
    }
    return buf;
}

JSONObject jsonBody(Map req, InputStream in) {
    byte[] body = readBody(req, in, MAX_JSON_BODY);
    if (body.length == 0) return new JSONObject();
    return new JSONObject(new String(body, "UTF-8"));
}

void sendJson(OutputStream out, int status, JSONObject j) {
    sendBytes(out, status, "application/json; charset=utf-8", j.toString().getBytes("UTF-8"));
}

String reason(int status) {
    if (status == 200) return "OK";
    if (status == 400) return "Bad Request";
    if (status == 401) return "Unauthorized";
    if (status == 404) return "Not Found";
    return "Internal Server Error";
}

void sendHead(OutputStream out, int status, String type, long length) {
    String head = "HTTP/1.1 " + status + " " + reason(status) + "\r\nContent-Type: " + type
        + "\r\nContent-Length: " + length + "\r\nConnection: close\r\n\r\n";
    out.write(head.getBytes("ISO-8859-1"));
}

void sendBytes(OutputStream out, int status, String type, byte[] body) {
    sendHead(out, status, type, body.length);
    out.write(body);
    out.flush();
}

String contentType(String name) {
    String n = name.toLowerCase();
    if (n.endsWith(".jpg") || n.endsWith(".jpeg")) return "image/jpeg";
    if (n.endsWith(".png")) return "image/png";
    if (n.endsWith(".gif")) return "image/gif";
    if (n.endsWith(".webp")) return "image/webp";
    return "application/octet-stream";
}

void sendFile(OutputStream out, File f) {
    sendHead(out, 200, contentType(f.getName()), f.length());
    FileInputStream in = new FileInputStream(f);
    try {
        byte[] buf = new byte[65536];
        int r;
        while ((r = in.read(buf)) > 0) out.write(buf, 0, r);
    } finally {
        in.close();
    }
    out.flush();
}

void requireAuth(Map req) {
    String h = (String) ((Map) req.get("headers")).get("authorization");
    String expected = "Bearer " + bridgeToken;
    if (h == null || !MessageDigest.isEqual(h.getBytes("UTF-8"), expected.getBytes("UTF-8"))) {
        throw new SecurityException("unauthorized");
    }
}

String q(Map req, String key, String def) {
    Object v = ((Map) req.get("query")).get(key);
    return v == null || ((String) v).length() == 0 ? def : (String) v;
}

String qRequired(Map req, String key) {
    String v = q(req, key, null);
    if (v == null) throw new IllegalArgumentException("missing query parameter " + key);
    return v;
}

long qLong(Map req, String key, long def) {
    String v = q(req, key, null);
    if (v == null) return def;
    try {
        return Long.parseLong(v);
    } catch (NumberFormatException e) {
        throw new IllegalArgumentException("bad " + key);
    }
}

List stringList(JSONObject j, String key) {
    List out = new ArrayList();
    Object v = j.opt(key);
    if (v instanceof JSONArray) {
        JSONArray a = (JSONArray) v;
        for (int i = 0; i < a.length(); i++) out.add(a.getString(i));
    } else if (v != null && v != JSONObject.NULL) {
        out.add(String.valueOf(v));
    }
    if (out.isEmpty()) throw new IllegalArgumentException("missing " + key);
    return out;
}

String optString(JSONObject j, String key) {
    return j.has(key) && !j.isNull(key) ? j.getString(key) : null;
}

// A local file argument must be something uploaded through /upload, so the API
// cannot be used to send arbitrary files off the device.
String uploadedPath(JSONObject j, String key) {
    String p = j.getString(key);
    File f = new File(p).getCanonicalFile();
    if (!f.getParent().equals(new File(uploadDir).getCanonicalPath()) || !f.isFile()) {
        throw new IllegalArgumentException(key + " must be a path returned by /upload");
    }
    return f.getAbsolutePath();
}

// ---------------------------------------------------------------------------
// Bridge: routes
// ---------------------------------------------------------------------------

Object route(Map req, InputStream in) {
    String method = (String) req.get("method");
    String path = (String) req.get("path");
    if (path.length() > 1 && path.endsWith("/")) path = path.substring(0, path.length() - 1);

    if (path.equals("/health")) {
        JSONObject j = ok();
        j.put("version", PLUGIN_VERSION);
        return j;
    }
    requireAuth(req);

    if (method.equals("GET")) {
        if (path.equals("/status")) return status();
        if (path.equals("/events")) return events(req);
        if (path.equals("/history")) {
            return messagesJson(queryHistoryMsg(qRequired(req, "talker"), qLong(req, "start", 0L),
                !q(req, "order", "asc").equals("desc"), (int) Math.min(qLong(req, "count", 100L), 1000L)));
        }
        if (path.equals("/message")) return findMessageJson(req);
        if (path.equals("/image")) return image(req);
        if (path.equals("/friends")) return beansJson(getFriendList());
        if (path.equals("/officials")) return beansJson(getOfficialList());
        if (path.equals("/groups")) return beansJson(getGroupList());
        if (path.equals("/group/members")) {
            String room = qRequired(req, "room");
            JSONObject j = ok();
            j.put("room", room);
            j.put("count", getGroupMemberCount(room));
            j.put("members", beansJson(getGroupMemberList(room)));
            return j;
        }
        if (path.equals("/contact")) return contact(req);
        if (path.equals("/labels")) return beansJson(getContactLabelList());
        if (path.equals("/label/contacts")) {
            String id = q(req, "id", null);
            return beansJson(id != null ? getContactByLabelId(id) : getContactByLabelName(qRequired(req, "name")));
        }
        throw new java.io.FileNotFoundException("no route GET " + path);
    }
    if (!method.equals("POST") && !method.equals("PUT")) throw new IllegalArgumentException("unsupported method " + method);
    if (path.equals("/upload")) return upload(req, in);
    JSONObject b = jsonBody(req, in);
    if (path.startsWith("/send/")) return send(path.substring(6), b);
    if (path.equals("/msg/revoke")) {
        revokeMsg(b.getLong("msg_id"));
        return ok();
    }
    if (path.equals("/msg/system")) {
        long createTime = b.has("create_time") ? b.getLong("create_time") : System.currentTimeMillis();
        JSONObject j = ok();
        j.put("msg_id", insertSystemMsg(b.getString("talker"), b.getString("content"), createTime));
        return j;
    }
    if (path.equals("/friend/verify")) {
        if (b.has("privacy")) verifyUser(b.getString("wxid"), b.getString("ticket"), b.getInt("scene"), b.getInt("privacy"));
        else verifyUser(b.getString("wxid"), b.getString("ticket"), b.getInt("scene"));
        return ok();
    }
    if (path.startsWith("/group/members/")) return groupMembers(path.substring(15), b);
    if (path.equals("/contact/labels")) {
        modifyContactLabelList(b.getString("wxid"), stringList(b, "labels"));
        return ok();
    }
    if (path.equals("/sns/post")) {
        String content = b.optString("content", "");
        if (b.has("images")) {
            JSONArray a = b.getJSONArray("images");
            List paths = new ArrayList();
            for (int i = 0; i < a.length(); i++) {
                JSONObject one = new JSONObject();
                one.put("p", a.getString(i));
                paths.add(uploadedPath(one, "p"));
            }
            uploadTextAndPicList(content, paths);
        } else {
            uploadText(content);
        }
        return ok();
    }
    if (path.equals("/audio/mp3-to-silk")) {
        String src = uploadedPath(b, "path");
        File dst = new File(uploadDir, System.currentTimeMillis() + "-" + new File(src).getName() + ".silk");
        JSONObject j = ok();
        j.put("code", mp3ToSilk(src, dst.getAbsolutePath()));
        j.put("path", dst.getAbsolutePath());
        return j;
    }
    if (path.equals("/toast")) {
        toast(b.getString("text"));
        return ok();
    }
    if (path.equals("/device-step")) {
        uploadDeviceStep(b.getLong("step"));
        return ok();
    }
    throw new java.io.FileNotFoundException("no route " + method + " " + path);
}

JSONObject status() {
    JSONObject j = ok();
    j.put("version", PLUGIN_VERSION);
    j.put("started_at", startedAt);
    j.put("last_seq", lastSeq.get());
    j.put("login_wxid", getLoginWxid());
    j.put("login_alias", getLoginAlias());
    j.put("host_ver_name", hostVerName);
    j.put("host_ver_code", hostVerCode);
    j.put("inbox_enabled", getBoolean("inbox_enabled", true));
    j.put("inbox_dir", inboxDir);
    j.put("bridge_dir", bridgeDir);
    return j;
}

String events(Map req) {
    long after = qLong(req, "after", 0L);
    int limit = (int) Math.max(1L, Math.min(qLong(req, "limit", 200L), 1000L));
    long wait = Math.max(0L, Math.min(qLong(req, "wait", 0L), 60L)) * 1000L;
    List lines = readEvents(after, limit, wait);
    // Built by hand: the lines are already JSON, so re-parsing them is wasted work.
    StringBuilder b = new StringBuilder("{\"ok\":true,\"last_seq\":").append(lastSeq.get()).append(",\"events\":[");
    for (int i = 0; i < lines.size(); i++) {
        if (i > 0) b.append(',');
        b.append((String) lines.get(i));
    }
    return b.append("]}").toString();
}

// The message `msg_id` in conversation `talker`; `create_time` (ms) narrows the
// history query to where the message is.
Object findMessage(Map req) {
    String talker = qRequired(req, "talker");
    long msgId = Long.parseLong(qRequired(req, "msg_id"));
    long createTime = qLong(req, "create_time", 0L);
    long start = createTime > 0 ? createTime - 1000L : 0L;
    for (int round = 0; round < 50; round++) {
        List page = queryHistoryMsg(talker, start, true, PAGE_SIZE);
        if (page == null || page.isEmpty()) break;
        long lastTime = start;
        for (int i = 0; i < page.size(); i++) {
            Object m = page.get(i);
            if (m.getMsgId() == msgId) return m;
            lastTime = Math.max(lastTime, m.getCreateTime());
        }
        if (page.size() < PAGE_SIZE || lastTime <= start || createTime > 0) break;
        start = lastTime;
    }
    throw new java.io.FileNotFoundException("message " + msgId + " not found in " + talker);
}

JSONObject findMessageJson(Map req) {
    JSONObject j = ok();
    j.put("message", messageJson(findMessage(req)));
    return j;
}

// The original of an image, either by message (talker + msg_id [+ create_time])
// or straight from its CDN fields (md5 + cdn_url + aes_key).
File image(Map req) {
    File tmp = new File(uploadDir, "img-" + System.nanoTime() + ".part");
    String md5 = q(req, "md5", null);
    if (md5 != null) {
        downloadImg(md5, qRequired(req, "cdn_url"), qRequired(req, "aes_key"), tmp.getAbsolutePath());
        if (!waitStable(tmp)) {
            tmp.delete();
            throw new RuntimeException("download produced no bytes");
        }
        return finishImage(tmp, "img-" + System.nanoTime());
    }
    Object msg = findMessage(req);
    if (!msg.isImage()) throw new IllegalArgumentException("message is not an image");
    String[] via = new String[1];
    File done = fetchOriginal(msg, tmp, via);
    if (done == null) throw new RuntimeException("download failed");
    return done;
}

JSONObject contact(Map req) {
    String wxid = qRequired(req, "wxid");
    String room = q(req, "room", null);
    JSONObject j = ok();
    j.put("wxid", wxid);
    j.put("nick_name", getFriendNickName(wxid));
    j.put("remark_name", getFriendRemarkName(wxid));
    j.put("name", room == null ? getFriendName(wxid) : getFriendName(wxid, room));
    if (room != null) j.put("display_name", getFriendDisplayName(wxid, room));
    j.put("avatar_url", getAvatarUrl(wxid, false));
    j.put("avatar_big_url", getAvatarUrl(wxid, true));
    return j;
}

JSONObject upload(Map req, InputStream in) {
    long len = ((Long) req.get("length")).longValue();
    if (len <= 0) throw new IllegalArgumentException("empty upload");
    if (len > MAX_UPLOAD) throw new IllegalArgumentException("upload too large");
    String name = q(req, "name", "upload.bin").replaceAll("[^A-Za-z0-9._-]", "_");
    pruneUploads();
    File dest = new File(uploadDir, System.currentTimeMillis() + "-" + name);
    FileOutputStream out = new FileOutputStream(dest);
    try {
        byte[] buf = new byte[65536];
        long left = len;
        while (left > 0) {
            int r = in.read(buf, 0, (int) Math.min(buf.length, left));
            if (r < 0) throw new IllegalArgumentException("truncated upload");
            out.write(buf, 0, r);
            left -= r;
        }
    } catch (Throwable e) {
        out.close();
        dest.delete();
        throw e;
    }
    out.close();
    JSONObject j = ok();
    j.put("path", dest.getAbsolutePath());
    j.put("size", dest.length());
    return j;
}

void pruneUploads() {
    File[] files = new File(uploadDir).listFiles();
    if (files == null) return;
    long cutoff = System.currentTimeMillis() - UPLOAD_TTL_MS;
    for (int i = 0; i < files.length; i++) {
        if (files[i].lastModified() < cutoff) files[i].delete();
    }
}

JSONObject groupMembers(String action, JSONObject b) {
    String room = b.getString("room");
    List members = stringList(b, "members");
    String reason = optString(b, "reason");
    if (action.equals("add")) {
        if (reason != null) addChatroomMember(room, members, reason);
        else addChatroomMember(room, members);
    } else if (action.equals("invite")) {
        if (reason != null) inviteChatroomMember(room, members, reason);
        else inviteChatroomMember(room, members);
    } else if (action.equals("remove")) {
        delChatroomMember(room, members);
    } else {
        throw new java.io.FileNotFoundException("no group action " + action);
    }
    return ok();
}

// Sends a text and waits briefly for WeChat's server id. The id is null when
// WeChat reports a failure or the callback does not arrive in time.
Object sendTextAndWait(String talker, String content) {
    final java.util.concurrent.CountDownLatch latch = new java.util.concurrent.CountDownLatch(1);
    final java.util.concurrent.atomic.AtomicReference ref = new java.util.concurrent.atomic.AtomicReference();
    sendText(talker, content, new java.util.function.Consumer() {
        public void accept(Object v) {
            ref.set(v);
            latch.countDown();
        }
    });
    latch.await(SEND_CALLBACK_MS, java.util.concurrent.TimeUnit.MILLISECONDS);
    return ref.get();
}

JSONObject send(String what, JSONObject b) {
    String talker = b.getString("talker");
    String appId = b.optString("app_id", "");
    JSONObject j = ok();
    if (what.equals("text")) {
        Object svrId = sendTextAndWait(talker, b.getString("content"));
        j.put("svr_id", svrId == null ? JSONObject.NULL : svrId);
    } else if (what.equals("quote")) {
        sendQuoteMsg(talker, b.getString("content"), b.getLong("msg_id"));
    } else if (what.equals("image")) {
        String p = uploadedPath(b, "path");
        if (b.has("quote_msg_id")) sendImage(talker, p, b.getLong("quote_msg_id"));
        else sendImage(talker, p);
    } else if (what.equals("video")) {
        sendVideo(talker, uploadedPath(b, "path"));
    } else if (what.equals("emoji")) {
        String p = uploadedPath(b, "path");
        if (b.has("quote_msg_id")) sendEmoji(talker, p, b.getLong("quote_msg_id"));
        else sendEmoji(talker, p);
    } else if (what.equals("voice")) {
        String p = uploadedPath(b, "path");
        if (b.has("duration")) sendVoice(talker, p, b.getInt("duration"));
        else sendVoice(talker, p);
    } else if (what.equals("file")) {
        String p = uploadedPath(b, "path");
        shareFile(talker, b.optString("title", new File(p).getName()), p, appId);
    } else if (what.equals("link")) {
        shareWebpage(talker, b.getString("title"), b.optString("description", ""), b.getString("url"), null, appId);
    } else if (what.equals("share-text")) {
        shareText(talker, b.getString("text"), appId);
    } else if (what.equals("share-video")) {
        shareVideo(talker, b.getString("title"), b.optString("description", ""), b.getString("url"), null, appId);
    } else if (what.equals("music")) {
        shareMusic(talker, b.getString("title"), b.optString("description", ""), b.getString("url"),
            b.getString("data_url"), null, appId);
    } else if (what.equals("mini-program")) {
        shareMiniProgram(talker, b.getString("title"), b.optString("description", ""), b.getString("user_name"),
            b.getString("page_path"), null, appId);
    } else if (what.equals("app-brand")) {
        sendAppBrandMsg(talker, b.getString("title"), b.getString("page_path"), b.getString("gh_name"));
    } else if (what.equals("pat")) {
        sendPat(talker, b.getString("user"));
    } else if (what.equals("card")) {
        sendShareCard(talker, b.getString("wxid"));
    } else if (what.equals("location")) {
        sendLocation(talker, b.getString("poi_name"), b.optString("label", ""), b.getString("x"), b.getString("y"),
            b.optString("scale", "16"));
    } else if (what.equals("note")) {
        sendNoteMsg(talker, b.getString("content"));
    } else if (what.equals("cipher")) {
        sendCipherMsg(talker, b.getString("title"), b.getString("content"));
    } else {
        throw new java.io.FileNotFoundException("no send type " + what);
    }
    return j;
}
