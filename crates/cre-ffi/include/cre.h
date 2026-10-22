#ifndef CRE_H
#define CRE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct CreDocument CreDocument;

enum CreMetadataField {
    CRE_METADATA_TITLE = 1,
    CRE_METADATA_CREATOR = 2,
    CRE_METADATA_LANGUAGE = 3
};

enum CreFormat {
    CRE_FORMAT_UNKNOWN = 0,
    CRE_FORMAT_EPUB = 1,
    CRE_FORMAT_FB2 = 2,
    CRE_FORMAT_TXT = 3
};

CreDocument *cre_open(const char *path);
void cre_close(CreDocument *doc);

int32_t cre_last_error(char *out, size_t out_len);

int32_t cre_page_count(const CreDocument *doc);
int32_t cre_current_page(const CreDocument *doc);
int32_t cre_goto_page(CreDocument *doc, uint32_t page);
int32_t cre_next_page(CreDocument *doc);
int32_t cre_previous_page(CreDocument *doc);
int32_t cre_first_page(CreDocument *doc);
int32_t cre_last_page(CreDocument *doc);

uint32_t cre_page_width(const CreDocument *doc);
uint32_t cre_page_height(const CreDocument *doc);

/* Returns the required byte count, or -1 on error. Writes only when out_len is large enough. */
ptrdiff_t cre_render_page(CreDocument *doc, uint32_t page, uint8_t *out, size_t out_len);

/* Returns the required byte count including the trailing NUL, or -1 on error. */
ptrdiff_t cre_page_text(const CreDocument *doc, uint32_t page, char *out, size_t out_len);

int32_t cre_format(const CreDocument *doc);
ptrdiff_t cre_metadata(const CreDocument *doc, uint32_t field, char *out, size_t out_len);

int32_t cre_toc_count(const CreDocument *doc);
ptrdiff_t cre_toc_title(const CreDocument *doc, uint32_t index, char *out, size_t out_len);
ptrdiff_t cre_toc_href(const CreDocument *doc, uint32_t index, char *out, size_t out_len);
int32_t cre_toc_page(const CreDocument *doc, uint32_t index);

int32_t cre_goto_href(CreDocument *doc, const char *href);
ptrdiff_t cre_xpointer_from_page(const CreDocument *doc, uint32_t page, char *out, size_t out_len);
int32_t cre_goto_xpointer(CreDocument *doc, const char *xpointer);

int32_t cre_find_text_count(const CreDocument *doc, const char *query);
int32_t cre_find_text_page(const CreDocument *doc, const char *query, uint32_t index);

#ifdef __cplusplus
}
#endif

#endif
