// M0 spike: read an Xcode index store via libIndexStore.dylib C API (declared by hand; Xcode ships no header).
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdbool.h>
#include <stdint.h>
#include <time.h>
typedef void *indexstore_t, *indexstore_error_t, *indexstore_unit_reader_t, *indexstore_record_reader_t,
             *indexstore_unit_dependency_t, *indexstore_occurrence_t, *indexstore_symbol_t;
typedef struct { const char *data; size_t length; } indexstore_string_ref_t;
indexstore_t indexstore_store_create(const char *, indexstore_error_t *);
const char *indexstore_error_get_description(indexstore_error_t);
bool indexstore_store_units_apply_f(indexstore_t, unsigned, void *, bool (*)(void *, indexstore_string_ref_t));
indexstore_unit_reader_t indexstore_unit_reader_create(indexstore_t, const char *, indexstore_error_t *);
bool indexstore_unit_reader_dependencies_apply_f(indexstore_unit_reader_t, void *, bool (*)(void *, indexstore_unit_dependency_t));
int indexstore_unit_dependency_get_kind(indexstore_unit_dependency_t);
indexstore_string_ref_t indexstore_unit_dependency_get_name(indexstore_unit_dependency_t);
void indexstore_unit_reader_dispose(indexstore_unit_reader_t);
indexstore_record_reader_t indexstore_record_reader_create(indexstore_t, const char *, indexstore_error_t *);
bool indexstore_record_reader_occurrences_apply_f(indexstore_record_reader_t, void *, bool (*)(void *, indexstore_occurrence_t));
bool indexstore_record_reader_symbols_apply_f(indexstore_record_reader_t, bool, void *, bool (*)(void *, indexstore_symbol_t));
void indexstore_record_reader_dispose(indexstore_record_reader_t);
uint64_t indexstore_occurrence_get_roles(indexstore_occurrence_t);

static char **recs; static size_t nrec, caprec;
static long units, symbols, occs, calls, defs;
static int seen(const char *s) { for (size_t i = 0; i < nrec; i++) if (!strcmp(recs[i], s)) return 1; return 0; }
static bool on_dep(void *ctx, indexstore_unit_dependency_t d) {
  if (indexstore_unit_dependency_get_kind(d) != 2) return true; // 2 = record
  indexstore_string_ref_t n = indexstore_unit_dependency_get_name(d);
  char *s = strndup(n.data, n.length);
  if (seen(s)) { free(s); return true; }
  if (nrec == caprec) recs = realloc(recs, (caprec = caprec ? caprec * 2 : 1024) * sizeof *recs);
  recs[nrec++] = s; return true;
}
static indexstore_t store;
static bool on_unit(void *ctx, indexstore_string_ref_t n) {
  char name[1024]; snprintf(name, sizeof name, "%.*s", (int)n.length, n.data);
  indexstore_error_t e = 0; indexstore_unit_reader_t u = indexstore_unit_reader_create(store, name, &e);
  if (!u) return true;
  units++; indexstore_unit_reader_dependencies_apply_f(u, 0, on_dep); indexstore_unit_reader_dispose(u); return true;
}
static bool on_sym(void *c, indexstore_symbol_t s) { symbols++; return true; }
static bool on_occ(void *c, indexstore_occurrence_t o) {
  uint64_t r = indexstore_occurrence_get_roles(o); occs++;
  if (r & (1 << 5)) calls++;      // SymbolRole::Call
  if (r & (1 << 1)) defs++;       // SymbolRole::Definition
  return true;
}
int main(int argc, char **argv) {
  struct timespec a, b; clock_gettime(CLOCK_MONOTONIC, &a);
  indexstore_error_t e = 0; store = indexstore_store_create(argv[1], &e);
  if (!store) { fprintf(stderr, "open failed: %s\n", indexstore_error_get_description(e)); return 1; }
  indexstore_store_units_apply_f(store, 0, 0, on_unit);
  for (size_t i = 0; i < nrec; i++) {
    indexstore_record_reader_t r = indexstore_record_reader_create(store, recs[i], &e);
    if (!r) continue;
    indexstore_record_reader_symbols_apply_f(r, true, 0, on_sym);
    indexstore_record_reader_occurrences_apply_f(r, 0, on_occ);
    indexstore_record_reader_dispose(r);
  }
  clock_gettime(CLOCK_MONOTONIC, &b);
  printf("units=%ld records=%zu symbols=%ld occurrences=%ld calls=%ld defs=%ld seconds=%.2f\n", units, nrec, symbols, occs, calls, defs,
         (b.tv_sec - a.tv_sec) + (b.tv_nsec - a.tv_nsec) / 1e9);
}
