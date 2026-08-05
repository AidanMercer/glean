// Extraction shim: poppler-cpp gives us correct glyphs, encodings and CID font
// handling. Everything above this line (layout, tables, markdown) is Rust.
//
// Each page is flattened into a word record array plus one text arena, so the
// Rust side crosses the FFI boundary once per page rather than once per word.

#include <poppler-document.h>
#include <poppler-page.h>
#include <poppler-global.h>

#include <cstring>
#include <memory>
#include <string>
#include <vector>

extern "C" {

struct GWord {
    double x0, y0, x1, y1;
    double fsize;
    unsigned int off;   // byte offset into the page arena
    unsigned int len;   // byte length
    int flags;          // bit0 bold, bit1 italic
};

struct GPage {
    std::vector<GWord> words;
    std::string arena;
    double width, height;
};

struct GDoc {
    std::unique_ptr<poppler::document> doc;
    std::vector<GPage> pages;
    std::vector<bool> loaded;
};

static std::string ustr_to_utf8(const poppler::ustring &u)
{
    poppler::byte_array b = u.to_utf8();
    return std::string(b.begin(), b.end());
}

GDoc *glean_open(const char *path)
{
    poppler::document *d = poppler::document::load_from_file(path);
    if (!d || d->is_locked()) {
        delete d;
        return nullptr;
    }
    GDoc *g = new GDoc();
    g->doc.reset(d);
    int n = d->pages();
    g->pages.resize(n);
    g->loaded.assign(n, false);
    return g;
}

int glean_pages(GDoc *g) { return g ? (int)g->pages.size() : 0; }

// Extract one page on demand. Returns word count, or -1 on failure.
int glean_load_page(GDoc *g, int idx)
{
    if (!g || idx < 0 || idx >= (int)g->pages.size()) return -1;
    if (g->loaded[idx]) return (int)g->pages[idx].words.size();

    std::unique_ptr<poppler::page> p(g->doc->create_page(idx));
    GPage &gp = g->pages[idx];
    g->loaded[idx] = true;
    if (!p) return 0;

    poppler::rectf pr = p->page_rect();
    gp.width = pr.width();
    gp.height = pr.height();

    // text_list() applies poppler's own word segmentation, the same path
    // pdftotext uses. The font flag is not free but heading detection and the
    // letter-spacing repair both key off glyph size, so it has to be on.
    std::vector<poppler::text_box> boxes =
        p->text_list(poppler::page::text_list_include_font);

    gp.words.reserve(boxes.size());
    for (const poppler::text_box &tb : boxes) {
        std::string s = ustr_to_utf8(tb.text());
        if (s.empty()) continue;
        // strip trailing whitespace poppler may attach to a word
        while (!s.empty() && (s.back() == ' ' || s.back() == '\n' || s.back() == '\t'))
            s.pop_back();
        if (s.empty()) continue;

        poppler::rectf r = tb.bbox();
        GWord w{};
        w.x0 = r.left();  w.y0 = r.top();
        w.x1 = r.right(); w.y1 = r.bottom();
        w.fsize = tb.has_font_info() ? tb.get_font_size() : (r.bottom() - r.top());
        w.flags = 0;
        if (tb.has_font_info()) {
            std::string fn = tb.get_font_name(0);
            for (auto &c : fn) c = (char)tolower((unsigned char)c);
            if (fn.find("bold") != std::string::npos || fn.find("black") != std::string::npos)
                w.flags |= 1;
            if (fn.find("italic") != std::string::npos || fn.find("oblique") != std::string::npos)
                w.flags |= 2;
        }
        w.off = (unsigned int)gp.arena.size();
        w.len = (unsigned int)s.size();
        gp.arena += s;
        gp.words.push_back(w);
    }
    return (int)gp.words.size();
}

const GWord *glean_words(GDoc *g, int idx, int *count)
{
    if (!g || idx < 0 || idx >= (int)g->pages.size()) { *count = 0; return nullptr; }
    *count = (int)g->pages[idx].words.size();
    return g->pages[idx].words.data();
}

const char *glean_arena(GDoc *g, int idx, int *len)
{
    if (!g || idx < 0 || idx >= (int)g->pages.size()) { *len = 0; return nullptr; }
    *len = (int)g->pages[idx].arena.size();
    return g->pages[idx].arena.data();
}

// Page dimensions independently of text extraction. Reading them off a loaded
// page only works if that page happens to have been loaded already — and it
// silently yields zero if not, which is the kind of failure that turns into a
// margin test that never matches. Fetch the rect on demand instead; it costs a
// page construction, not a text pass.
void glean_page_size(GDoc *g, int idx, double *w, double *h)
{
    if (!g || idx < 0 || idx >= (int)g->pages.size()) { *w = *h = 0; return; }
    GPage &gp = g->pages[idx];
    if (gp.width <= 0.0 || gp.height <= 0.0) {
        std::unique_ptr<poppler::page> p(g->doc->create_page(idx));
        if (p) {
            poppler::rectf r = p->page_rect();
            gp.width = r.width();
            gp.height = r.height();
        }
    }
    *w = gp.width;
    *h = gp.height;
}

// Free a page's buffers once Rust has consumed it, so peak memory stays flat
// on a 300-page document instead of growing with page count.
void glean_release_page(GDoc *g, int idx)
{
    if (!g || idx < 0 || idx >= (int)g->pages.size()) return;
    GPage &gp = g->pages[idx];
    std::vector<GWord>().swap(gp.words);
    std::string().swap(gp.arena);
}

void glean_close(GDoc *g) { delete g; }

} // extern "C"
