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

#include <zlib.h>

#include <cmath>
#include <cstdio>
#include <cstring>
#include <string>
#include <vector>

namespace {

struct Rec {
    int page;
    double x0, y0, x1, y1;   // placement on the page, PDF points
    int w, h;                // pixel dimensions
    std::string path;
};

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
    ImageGrab(std::string dir, int minPx) : dir_(std::move(dir)), minPx_(minPx) {}

    bool interpretType3Chars() override { return false; }
    bool upsideDown() override { return true; }
    bool useDrawChar() override { return false; }
    void setPage(int p) { page_ = p; }
    std::vector<Rec> &recs() { return recs_; }

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

        char name[64];
        snprintf(name, sizeof(name), "p%03d-%02d.png", page_, (int)recs_.size() + 1);
        std::string path = dir_ + "/" + name;
        if (write_png(path, width, height, rgb))
            recs_.push_back(Rec{page_, x0, y0, x1, y1, width, height, path});
    }

private:
    std::string dir_;
    int minPx_;
    int page_ = 0;
    std::vector<Rec> recs_;
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
};

// Extract every image at or above minPx on a side. Returns null on failure.
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

int glean_images_count(GImages *g) { return g ? (int)g->view.size() : 0; }
const GImage *glean_images_data(GImages *g) { return g && !g->view.empty() ? g->view.data() : nullptr; }
void glean_images_free(GImages *g) { delete g; }

} // extern "C"
