// Embedded-image extraction.
//
// poppler-cpp has no image API, so this drops to poppler's core OutputDev and
// captures every image the page draws, together with the CTM that places it —
// the placement matters as much as the pixels, because a page-sized image is a
// scan backdrop and a small one is a photograph, and only the box tells them
// apart.
//
// Images are decoded to RGB8 and written as PNG (zlib is already a poppler
// dependency), so one format comes out regardless of what went in.

#include <PDFDoc.h>
#include <GlobalParams.h>
#include <OutputDev.h>
#include <GfxState.h>
#include <Stream.h>
#include <Page.h>

#include <poppler-document.h>
#include <poppler-page.h>
#include <poppler-page-renderer.h>
#include <poppler-image.h>

#include <zlib.h>

#include <cmath>
#include <cstdio>
#include <cstring>
#include <algorithm>
#include <map>
#include <string>
#include <array>
#include <vector>

namespace {

struct Rec {
    int page;
    double x0, y0, x1, y1;   // placement on the page, PDF points
    int w, h;                // pixel dimensions
    std::string path;
};

// FNV-1a over the decoded pixels. A logo repeated on every page decodes to the
// same bytes, so hashing the RGB buffer (not the embedded stream, which may be
// re-encoded per placement) collapses them to one file.
static unsigned long long pixel_hash(const std::vector<unsigned char> &v)
{
    unsigned long long h = 1469598103934665603ULL;
    for (unsigned char c : v) {
        h ^= c;
        h *= 1099511628211ULL;
    }
    return h;
}

void be32(std::string &s, unsigned int v)
{
    s.push_back((char)((v >> 24) & 0xff));
    s.push_back((char)((v >> 16) & 0xff));
    s.push_back((char)((v >> 8) & 0xff));
    s.push_back((char)(v & 0xff));
}

void chunk(std::string &out, const char *tag, const std::string &data)
{
    be32(out, (unsigned int)data.size());
    std::string body(tag);
    body += data;
    out += body;
    be32(out, (unsigned int)crc32(0, (const Bytef *)body.data(), (uInt)body.size()));
}

// Minimal PNG writer: 8-bit RGB, one IDAT.
bool write_png(const std::string &path, int w, int h, const std::vector<unsigned char> &rgb)
{
    if (w <= 0 || h <= 0 || rgb.size() < (size_t)w * h * 3) return false;

    std::string raw;
    raw.reserve((size_t)h * (1 + (size_t)w * 3));
    for (int y = 0; y < h; y++) {
        raw.push_back(0); // filter: none
        raw.append((const char *)&rgb[(size_t)y * w * 3], (size_t)w * 3);
    }

    uLongf clen = compressBound((uLong)raw.size());
    std::vector<unsigned char> comp(clen);
    if (compress2(comp.data(), &clen, (const Bytef *)raw.data(), (uLong)raw.size(), 6) != Z_OK)
        return false;

    std::string png("\x89PNG\r\n\x1a\n", 8);
    std::string ihdr;
    be32(ihdr, (unsigned int)w);
    be32(ihdr, (unsigned int)h);
    ihdr.push_back(8);      // bit depth
    ihdr.push_back(2);      // colour type: truecolour
    ihdr.append(3, '\0');   // compression, filter, interlace
    chunk(png, "IHDR", ihdr);
    chunk(png, "IDAT", std::string((const char *)comp.data(), clen));
    chunk(png, "IEND", std::string());

    FILE *f = fopen(path.c_str(), "wb");
    if (!f) return false;
    bool ok = fwrite(png.data(), 1, png.size(), f) == png.size();
    fclose(f);
    return ok;
}

class ImageGrab : public OutputDev {
public:
    // probe: record where every raster sits and stop there — no pixel decode, no
    // PNG. Classifying a wordless page (scan? figure? genuinely blank?) needs the
    // box and nothing else, and decoding a 300-dpi page scan to answer that is
    // most of the cost of extraction for none of the answer.
    ImageGrab(std::string dir, int minPx, bool probe = false)
        : dir_(std::move(dir)), minPx_(minPx), probe_(probe) {}

    bool interpretType3Chars() override { return false; }
    bool upsideDown() override { return true; }
    bool useDrawChar() override { return false; }
    void setPage(int p) { page_ = p; }
    std::vector<Rec> &recs() { return recs_; }
    std::vector<std::array<double, 4>> &paths() { return paths_; }

    // A chart drawn with path operators is not an image and never reaches
    // drawImage. Record where ink lands so those regions can be found later.
    void stroke(GfxState *s) override { notePath(s); }
    void fill(GfxState *s) override { notePath(s); }
    void eoFill(GfxState *s) override { notePath(s); }

    void drawImage(GfxState *state, Object * /*ref*/, Stream *str, int width, int height,
                   GfxImageColorMap *colorMap, bool /*interpolate*/,
                   const int * /*maskColors*/, bool /*inlineImg*/) override
    {
        if (!colorMap || !colorMap->isOk()) return;
        if (width < minPx_ || height < minPx_) return;

        // The CTM maps the unit square onto the page, so its corners give the box.
        const auto &m = state->getCTM();
        double xs[4] = {m[4], m[0] + m[4], m[2] + m[4], m[0] + m[2] + m[4]};
        double ys[4] = {m[5], m[1] + m[5], m[3] + m[5], m[1] + m[3] + m[5]};
        double x0 = xs[0], x1 = xs[0], y0 = ys[0], y1 = ys[0];
        for (int i = 1; i < 4; i++) {
            x0 = std::fmin(x0, xs[i]); x1 = std::fmax(x1, xs[i]);
            y0 = std::fmin(y0, ys[i]); y1 = std::fmax(y1, ys[i]);
        }

        // Probe: the box is the whole answer. Bail before touching the stream.
        if (probe_) {
            recs_.push_back(Rec{page_, x0, y0, x1, y1, width, height, std::string()});
            return;
        }

        ImageStream *imgStr =
            new ImageStream(str, width, colorMap->getNumPixelComps(), colorMap->getBits());
        if (!imgStr->rewind()) { delete imgStr; return; }

        std::vector<unsigned char> rgb((size_t)width * height * 3, 0);
        for (int y = 0; y < height; y++) {
            unsigned char *row = imgStr->getLine();
            if (!row) break;
            for (int x = 0; x < width; x++) {
                GfxRGB c;
                colorMap->getRGB(row + (size_t)x * colorMap->getNumPixelComps(), &c);
                size_t o = ((size_t)y * width + x) * 3;
                rgb[o]     = (unsigned char)(colToByte(c.r));
                rgb[o + 1] = (unsigned char)(colToByte(c.g));
                rgb[o + 2] = (unsigned char)(colToByte(c.b));
            }
        }
        imgStr->close();
        delete imgStr;

        // Same pixels seen before: record the placement, reuse the file.
        unsigned long long hash = pixel_hash(rgb);
        auto seen = byHash_.find(hash);
        if (seen != byHash_.end()) {
            recs_.push_back(Rec{page_, x0, y0, x1, y1, width, height, seen->second});
            return;
        }

        char name[64];
        snprintf(name, sizeof(name), "p%03d-%02d.png", page_, ++written_);
        std::string path = dir_ + "/" + name;
        if (write_png(path, width, height, rgb)) {
            byHash_[hash] = path;
            recs_.push_back(Rec{page_, x0, y0, x1, y1, width, height, path});
        }
    }

private:
    void notePath(GfxState *state)
    {
        const GfxPath *path = state->getPath();
        if (!path) return;
        double x0 = 1e18, y0 = 1e18, x1 = -1e18, y1 = -1e18;
        for (int i = 0; i < path->getNumSubpaths(); i++) {
            const GfxSubpath *sp = path->getSubpath(i);
            for (int k = 0; k < sp->getNumPoints(); k++) {
                double dx, dy;
                state->transform(sp->getX(k), sp->getY(k), &dx, &dy);
                x0 = std::fmin(x0, dx); x1 = std::fmax(x1, dx);
                y0 = std::fmin(y0, dy); y1 = std::fmax(y1, dy);
            }
        }
        if (x1 <= x0 || y1 <= y0) return;
        paths_.push_back({(double)page_, x0, y0, x1});
        pathBoxes_.push_back({x0, y0, x1, y1, (double)page_});
    }

    std::string dir_;
    int minPx_;
    bool probe_ = false;
    int page_ = 0;
    int written_ = 0;
    std::vector<Rec> recs_;
    std::map<unsigned long long, std::string> byHash_;
    std::vector<std::array<double, 4>> paths_;

public:
    std::vector<std::array<double, 5>> pathBoxes_;  // x0,y0,x1,y1,page
};

} // namespace

extern "C" {

struct GImage {
    int page;
    double x0, y0, x1, y1;
    int w, h;
    const char *path;
};

struct GImages {
    std::vector<Rec> recs;
    std::vector<GImage> view;
    std::vector<int> ink;   // pages carrying path ink (probe only)
};

// Extract every image at or above minPx on a side. Returns null on failure.
// Merge overlapping or near-touching boxes on the same page until nothing more
// merges. A chart is hundreds of separate strokes; only the union is a figure.
static std::vector<std::array<double, 5>> cluster(std::vector<std::array<double, 5>> b, double pad)
{
    bool merged = true;
    while (merged) {
        merged = false;
        for (size_t i = 0; i < b.size(); i++) {
            for (size_t j = i + 1; j < b.size();) {
                bool same = b[i][4] == b[j][4];
                bool hit = same && b[i][0] - pad <= b[j][2] && b[j][0] - pad <= b[i][2]
                                && b[i][1] - pad <= b[j][3] && b[j][1] - pad <= b[i][3];
                if (hit) {
                    b[i][0] = std::fmin(b[i][0], b[j][0]);
                    b[i][1] = std::fmin(b[i][1], b[j][1]);
                    b[i][2] = std::fmax(b[i][2], b[j][2]);
                    b[i][3] = std::fmax(b[i][3], b[j][3]);
                    b.erase(b.begin() + j);
                    merged = true;
                } else {
                    j++;
                }
            }
        }
    }
    return b;
}

// Render vector-drawn figures: cluster the ink, then rasterise each region.
// Returns the number written.
int glean_figures(const char *pdf, const char *outdir, int firstPage, int lastPage,
                  double minSidePts, double dpi, GImages *into)
{
    globalParams = std::make_unique<GlobalParams>();
    auto core = std::make_unique<PDFDoc>(std::make_unique<GooString>(pdf));
    if (!core->isOk()) return 0;

    ImageGrab dev(outdir, 1 << 20);   // huge minPx: collect paths only, no images
    int last = lastPage > 0 ? lastPage : core->getNumPages();
    for (int p = firstPage; p <= last && p <= core->getNumPages(); p++) {
        dev.setPage(p);
        core->displayPage(&dev, p, 72.0, 72.0, 0, false, false, false);
    }

    auto boxes = cluster(dev.pathBoxes_, 6.0);
    std::unique_ptr<poppler::document> pdoc(poppler::document::load_from_file(pdf));
    if (!pdoc) return 0;
    poppler::page_renderer r;
    r.set_render_hint(poppler::page_renderer::antialiasing, true);
    r.set_render_hint(poppler::page_renderer::text_antialiasing, true);

    int n = 0;
    for (const auto &b : boxes) {
        double w = b[2] - b[0], h = b[3] - b[1];
        if (w < minSidePts || h < minSidePts) continue;
        int pg = (int)b[4];
        std::unique_ptr<poppler::page> pp(pdoc->create_page(pg - 1));
        if (!pp) continue;
        // Page furniture — background panels, header bars, borders — are paths
        // too, and they bridge every other cluster into one page-sized blob.
        // A figure occupies part of a page; anything larger is the page itself.
        poppler::rectf pr = pp->page_rect();
        double area = pr.width() * pr.height();
        if (area > 0 && (w * h) > 0.55 * area) continue;
        double sc = dpi / 72.0;
        // Honour the page's own /Rotate, or a landscape scan comes out sideways
        // and is useless to a vision model downstream.
        poppler::rotation_enum rot = poppler::rotate_0;
        switch (pp->orientation()) {
        case poppler::page::landscape:  rot = poppler::rotate_90;  break;
        case poppler::page::upside_down: rot = poppler::rotate_180; break;
        case poppler::page::seascape:   rot = poppler::rotate_270; break;
        default: break;
        }
        poppler::image im = r.render_page(pp.get(), dpi, dpi,
                                          (int)(b[0] * sc), (int)(b[1] * sc),
                                          (int)(w * sc), (int)(h * sc), rot);
        if (!im.is_valid()) continue;

        std::vector<unsigned char> rgb((size_t)im.width() * im.height() * 3, 255);
        for (int y = 0; y < im.height(); y++) {
            const unsigned char *row = (const unsigned char *)im.const_data() + (size_t)y * im.bytes_per_row();
            for (int x = 0; x < im.width(); x++) {
                size_t o = ((size_t)y * im.width() + x) * 3;
                // poppler renders BGRA/BGR little-endian; swap to RGB
                rgb[o] = row[x * 4 + 2];
                rgb[o + 1] = row[x * 4 + 1];
                rgb[o + 2] = row[x * 4];
            }
        }
        char name[64];
        snprintf(name, sizeof(name), "fig-p%03d-%02d.png", pg, ++n);
        std::string path = std::string(outdir) + "/" + name;
        if (write_png(path, im.width(), im.height(), rgb) && into) {
            into->recs.push_back(Rec{pg, b[0], b[1], b[2], b[3], im.width(), im.height(), path});
        }
    }
    if (into) {
        into->view.clear();
        for (const auto &rr : into->recs)
            into->view.push_back(GImage{rr.page, rr.x0, rr.y0, rr.x1, rr.y1, rr.w, rr.h, rr.path.c_str()});
    }
    return n;
}

// Render whole pages to PNG — the reply to "these pages are scans, they need
// OCR". Rendering rather than lifting the embedded raster is deliberate: a
// scanned page is sometimes several tiles, sometimes carries a stamp or a
// signature block drawn over the scan, and the page is what a human sees.
//
// `dpis` is per page, because a scan already has a resolution and the caller
// knows it: re-rendering a 300 dpi fax at 150 throws away half of what OCR has
// to work with, and rendering it at 600 invents nothing and costs four times
// the bytes.
int glean_render_pages(const char *pdf, const char *outdir, const int *pages, int n,
                       const double *dpis)
{
    globalParams = std::make_unique<GlobalParams>();
    std::unique_ptr<poppler::document> pdoc(poppler::document::load_from_file(pdf));
    if (!pdoc) return 0;

    poppler::page_renderer r;
    r.set_render_hint(poppler::page_renderer::antialiasing, true);
    r.set_render_hint(poppler::page_renderer::text_antialiasing, true);

    int written = 0;
    for (int i = 0; i < n; i++) {
        int pg = pages[i];
        std::unique_ptr<poppler::page> pp(pdoc->create_page(pg - 1));
        if (!pp) continue;
        // Honour the page's own /Rotate, or a landscape scan reaches the OCR
        // engine sideways and comes back as nothing.
        poppler::rotation_enum rot = poppler::rotate_0;
        switch (pp->orientation()) {
        case poppler::page::landscape:   rot = poppler::rotate_90;  break;
        case poppler::page::upside_down: rot = poppler::rotate_180; break;
        case poppler::page::seascape:    rot = poppler::rotate_270; break;
        default: break;
        }
        double dpi = dpis[i];
        poppler::image im = r.render_page(pp.get(), dpi, dpi, -1, -1, -1, -1, rot);
        if (!im.is_valid()) continue;

        std::vector<unsigned char> rgb((size_t)im.width() * im.height() * 3, 255);
        for (int y = 0; y < im.height(); y++) {
            const unsigned char *row =
                (const unsigned char *)im.const_data() + (size_t)y * im.bytes_per_row();
            for (int x = 0; x < im.width(); x++) {
                size_t o = ((size_t)y * im.width() + x) * 3;
                rgb[o]     = row[x * 4 + 2];   // poppler renders BGRA
                rgb[o + 1] = row[x * 4 + 1];
                rgb[o + 2] = row[x * 4];
            }
        }
        char name[64];
        snprintf(name, sizeof(name), "page-%04d.png", pg);
        if (write_png(std::string(outdir) + "/" + name, im.width(), im.height(), rgb)) written++;
    }
    return written;
}

GImages *glean_images(const char *pdf, const char *outdir, int firstPage, int lastPage, int minPx)
{
    globalParams = std::make_unique<GlobalParams>();
    auto doc = std::make_unique<PDFDoc>(std::make_unique<GooString>(pdf));
    if (!doc->isOk()) return nullptr;

    auto *g = new GImages();
    ImageGrab dev(outdir, minPx);
    int last = lastPage > 0 ? lastPage : doc->getNumPages();
    for (int p = firstPage; p <= last && p <= doc->getNumPages(); p++) {
        dev.setPage(p);
        doc->displayPage(&dev, p, 72.0, 72.0, 0, false, false, false);
    }
    g->recs = dev.recs();
    for (const auto &r : g->recs)
        g->view.push_back(GImage{r.page, r.x0, r.y0, r.x1, r.y1, r.w, r.h, r.path.c_str()});
    return g;
}

// Survey a page range without writing anything: every raster's box, plus which
// pages carry path ink. This is what tells a wordless page apart — a page-sized
// raster is a scan and needs OCR, a small one is a figure, ink with no raster is
// vector art, and none of the three is a blank page. Saying "42 pages need OCR"
// when some of them are blank is the same class of untruth as saying nothing at
// all, so the distinction is drawn here rather than guessed downstream.
GImages *glean_images_probe(const char *pdf, int firstPage, int lastPage, int minPx)
{
    globalParams = std::make_unique<GlobalParams>();
    auto doc = std::make_unique<PDFDoc>(std::make_unique<GooString>(pdf));
    if (!doc->isOk()) return nullptr;

    auto *g = new GImages();
    ImageGrab dev("", minPx, true);
    int last = lastPage > 0 ? lastPage : doc->getNumPages();
    for (int p = firstPage; p <= last && p <= doc->getNumPages(); p++) {
        dev.setPage(p);
        doc->displayPage(&dev, p, 72.0, 72.0, 0, false, false, false);
    }
    g->recs = dev.recs();
    for (const auto &r : g->recs)
        g->view.push_back(GImage{r.page, r.x0, r.y0, r.x1, r.y1, r.w, r.h, nullptr});
    for (const auto &b : dev.pathBoxes_) {
        int pg = (int)b[4];
        if (g->ink.empty() || g->ink.back() != pg) g->ink.push_back(pg);
    }
    std::sort(g->ink.begin(), g->ink.end());
    g->ink.erase(std::unique(g->ink.begin(), g->ink.end()), g->ink.end());
    return g;
}

int glean_ink_count(GImages *g) { return g ? (int)g->ink.size() : 0; }
const int *glean_ink_data(GImages *g) { return g && !g->ink.empty() ? g->ink.data() : nullptr; }

int glean_images_count(GImages *g) { return g ? (int)g->view.size() : 0; }
const GImage *glean_images_data(GImages *g) { return g && !g->view.empty() ? g->view.data() : nullptr; }
void glean_images_free(GImages *g) { delete g; }

} // extern "C"
