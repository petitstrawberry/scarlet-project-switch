/* SPDX-License-Identifier: GPL-2.0-only */
/* Host-only SGFX NIR -> GM20B SASS producer. No DRM device is opened.
 * Shader headers/slot assignment follow pinned Mesa nvc0_program.c.
 * See NOTICE for the Nouveau authors' MIT copyright and permission. */
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include "compiler/glsl_types.h"
#include "compiler/nir/nir_builder.h"
#include "pipe/p_shader_tokens.h"
#include "nv50_ir_driver.h"

enum fs_kind { FS_SOLID, FS_VERTEX_COLOR, FS_TEXTURE_RGBA, FS_TEXTURE_ALPHA_MASK,
               FS_TEXTURE_VERTEX_COLOR_RGBA, FS_TEXTURE_RGB_IGNORE_ALPHA };

static nir_def *
load_const(nir_builder *b, unsigned base, unsigned components)
{
   return nir_load_ubo(b, components, 32, nir_imm_int(b, 0),
                       nir_imm_int(b, base * 4), .align_mul = 16,
                       .align_offset = 0, .range_base = base * 4,
                       .range = components * 4);
}

static nir_def *
matrix_transform(nir_builder *b, nir_def *position)
{
   nir_def *c0 = load_const(b, 0, 4);
   nir_def *c1 = load_const(b, 4, 4);
   nir_def *c2 = load_const(b, 8, 4);
   nir_def *c3 = load_const(b, 12, 4);
   nir_def *r = nir_fmul(b, c0, nir_channel(b, position, 0));
   r = nir_fadd(b, nir_fmul(b, c1, nir_channel(b, position, 1)), r);
   r = nir_fadd(b, nir_fmul(b, c2, nir_channel(b, position, 2)), r);
   return nir_fadd(b, nir_fmul(b, c3, nir_channel(b, position, 3)), r);
}

static nir_variable *
io_var(nir_builder *b, nir_variable_mode mode, unsigned location,
       const struct glsl_type *type)
{
   return nir_create_variable_with_location(b->shader, mode, location, type);
}

static nir_shader *
build_vs(const char *name, bool position_is_vec2, bool has_color, bool color_is_vec3,
         bool has_uv)
{
   nir_builder nb = nir_builder_init_simple_shader(MESA_SHADER_VERTEX, NULL, "%s", name);
   nir_builder *b = &nb;
   nir_variable *in_pos = io_var(b, nir_var_shader_in, VERT_ATTRIB_GENERIC0,
                                 position_is_vec2 ? glsl_vec2_type() : glsl_vec4_type());
   nir_def *p = nir_load_var(b, in_pos);
   if (position_is_vec2)
      p = nir_vec4(b, nir_channel(b, p, 0), nir_channel(b, p, 1),
                   nir_imm_float(b, 0.0f), nir_imm_float(b, 1.0f));
   nir_variable *out_pos = io_var(b, nir_var_shader_out, VARYING_SLOT_POS,
                                  glsl_vec4_type());
   nir_store_var(b, out_pos, matrix_transform(b, p), 0xf);

   if (has_color) {
      nir_variable *in = io_var(b, nir_var_shader_in, VERT_ATTRIB_GENERIC1,
                                color_is_vec3 ? glsl_vec_type(3) : glsl_vec4_type());
      nir_variable *out = io_var(b, nir_var_shader_out, VARYING_SLOT_VAR0,
                                 glsl_vec4_type());
      nir_def *c = nir_load_var(b, in);
      if (color_is_vec3)
         c = nir_vec4(b, nir_channel(b, c, 0), nir_channel(b, c, 1),
                      nir_channel(b, c, 2), nir_imm_float(b, 1.0f));
      nir_store_var(b, out, c, 0xf);
   }
   if (has_uv) {
      unsigned attr = has_color ? VERT_ATTRIB_GENERIC2 : VERT_ATTRIB_GENERIC1;
      unsigned slot = has_color ? VARYING_SLOT_VAR1 : VARYING_SLOT_VAR0;
      nir_variable *in = io_var(b, nir_var_shader_in, attr, glsl_vec2_type());
      nir_variable *out = io_var(b, nir_var_shader_out, slot, glsl_vec2_type());
      nir_store_var(b, out, nir_load_var(b, in), 0x3);
   }
   return b->shader;
}

static nir_def *
sample_texture(nir_builder *b, nir_def *uv)
{
   b->shader->info.num_textures = 1;
   BITSET_SET(b->shader->info.textures_used, 0);
   BITSET_SET(b->shader->info.samplers_used, 0);
   return nir_tex(b, uv, .texture_index = 0, .sampler_index = 0,
                  .dim = GLSL_SAMPLER_DIM_2D, .dest_type = nir_type_float32);
}

static nir_shader *
build_fs(const char *name, enum fs_kind kind)
{
   nir_builder nb = nir_builder_init_simple_shader(MESA_SHADER_FRAGMENT, NULL, "%s", name);
   nir_builder *b = &nb;
   nir_def *color = load_const(b, 16, 4);
   nir_def *vertex_color = NULL, *uv = NULL, *sample = NULL, *result = NULL;
   if (kind == FS_VERTEX_COLOR || kind == FS_TEXTURE_VERTEX_COLOR_RGBA) {
      nir_variable *in = io_var(b, nir_var_shader_in, VARYING_SLOT_VAR0,
                                glsl_vec4_type());
      vertex_color = nir_load_var(b, in);
   }
   if (kind == FS_TEXTURE_RGBA || kind == FS_TEXTURE_ALPHA_MASK ||
       kind == FS_TEXTURE_RGB_IGNORE_ALPHA) {
      nir_variable *in = io_var(b, nir_var_shader_in, VARYING_SLOT_VAR0,
                                glsl_vec2_type());
      uv = nir_load_var(b, in);
   } else if (kind == FS_TEXTURE_VERTEX_COLOR_RGBA) {
      nir_variable *in = io_var(b, nir_var_shader_in, VARYING_SLOT_VAR1,
                                glsl_vec2_type());
      uv = nir_load_var(b, in);
   }
   if (uv)
      sample = sample_texture(b, uv);

   switch (kind) {
   case FS_SOLID:
      result = color;
      break;
   case FS_VERTEX_COLOR:
      result = nir_fmul(b, vertex_color, color);
      break;
   case FS_TEXTURE_RGBA:
      result = nir_fmul(b, sample, color);
      break;
   case FS_TEXTURE_ALPHA_MASK:
      result = nir_vec4(b, nir_channel(b, color, 0), nir_channel(b, color, 1),
                        nir_channel(b, color, 2),
                        nir_fmul(b, nir_channel(b, sample, 3),
                                 nir_channel(b, color, 3)));
      break;
   case FS_TEXTURE_VERTEX_COLOR_RGBA:
      result = nir_fmul(b, nir_fmul(b, sample, vertex_color), color);
      break;
   case FS_TEXTURE_RGB_IGNORE_ALPHA:
      result = nir_fmul(b,
                        nir_vec4(b, nir_channel(b, sample, 0),
                                 nir_channel(b, sample, 1),
                                 nir_channel(b, sample, 2),
                                 nir_imm_float(b, 1.0f)),
                        color);
      break;
   }
   nir_variable *out = io_var(b, nir_var_shader_out, FRAG_RESULT_DATA0,
                              glsl_vec4_type());
   nir_store_var(b, out, result, 0xf);
   return b->shader;
}

static unsigned
varying_address(const struct nv50_ir_varying *v)
{
   switch (v->sn) {
   case TGSI_SEMANTIC_POSITION: return 0x70;
   case TGSI_SEMANTIC_GENERIC: return 0x80 + 0x10 * v->si;
   default: fprintf(stderr, "unsupported shader semantic %u\n", v->sn); exit(2);
   }
}

static int
assign_slots(struct nv50_ir_prog_info_out *info)
{
   for (unsigned i = 0; i < info->numInputs; i++) {
      unsigned base = info->type == MESA_SHADER_VERTEX ? 0x80 + 0x10 * i
                                                      : varying_address(&info->in[i]);
      for (unsigned c = 0; c < 4; c++) info->in[i].slot[c] = base / 4 + c;
   }
   for (unsigned i = 0; i < info->numOutputs; i++) {
      unsigned base = 0;
      if (info->type == MESA_SHADER_FRAGMENT) {
         if (info->out[i].sn != TGSI_SEMANTIC_COLOR || info->out[i].si != 0) return -1;
      } else base = varying_address(&info->out[i]);
      for (unsigned c = 0; c < 4; c++) info->out[i].slot[c] = base / 4 + c;
   }
   return 0;
}

static void
make_header(const struct nv50_ir_prog_info_out *o, uint32_t h[20])
{
   memset(h, 0, 80);
   if (o->type == MESA_SHADER_VERTEX) {
      h[0] = 0x20061 | (1 << 10);
      h[4] = 0xff000;
      for (unsigned i = 0; i < o->numInputs; i++) {
         for (unsigned c = 0; c < 4; c++) {
            unsigned a = o->in[i].slot[c];
            if (o->in[i].mask & (1 << c)) h[5 + a / 32] |= 1u << (a % 32);
         }
      }
      for (unsigned i = 0; i < o->numOutputs; i++) {
         for (unsigned c = 0; c < 4; c++) {
            if (!(o->out[i].mask & (1 << c))) continue;
            unsigned a = o->out[i].slot[c] - 0x40 / 4;
            if (o->out[i].oread || o->out[i].slot[c] < 0x40 / 4 || 13 + a / 32 >= 20) {
               fprintf(stderr, "unsupported vertex header output\n"); exit(2);
            }
            h[13 + a / 32] |= 1u << (a % 32);
         }
      }
   } else {
      h[0] = 0x20062 | (5 << 10);
      h[5] = 0x80000000;
      if (!o->prop.fp.separateFragData) h[0] |= 0x4000;
      for (unsigned i = 0; i < o->numInputs; i++) {
         // These fixed SGFX variants have generic perspective varyings.
         if (o->in[i].sn != TGSI_SEMANTIC_GENERIC || o->in[i].flat || o->in[i].linear) {
            fprintf(stderr, "unsupported fragment interpolation\n"); exit(2);
         }
         for (unsigned c = 0; c < 4; c++) {
            if (!(o->in[i].mask & (1 << c))) continue;
            unsigned a = o->in[i].slot[c] * 2;
            if (4 + a / 32 >= 20) exit(2);
            h[4 + a / 32] |= 2u << (a % 32);
         }
      }
      h[18] = 0xf;
   }
}

static void
write_blob(const char *directory, const char *name, const char *suffix,
           const void *data, size_t size)
{
   char path[1024];
   if (snprintf(path, sizeof(path), "%s/%s%s", directory, name, suffix) >= sizeof(path)) exit(2);
   FILE *f = fopen(path, "wb");
   if (!f || fwrite(data, 1, size, f) != size || fclose(f)) { perror(path); exit(2); }
}

static void
compile_one(const char *directory, FILE *metadata, const char *name,
            nir_shader *nir, bool last)
{
   nir->options = nv50_ir_nir_shader_compiler_options(0x12b, nir->info.stage);
   nir_assign_io_var_locations(nir, nir_var_shader_in);
   nir_assign_io_var_locations(nir, nir_var_shader_out);
   nir_shader_gather_info(nir, nir_shader_get_entrypoint(nir));
   struct nv50_ir_prog_info info = {0};
   info.target = 0x12b;
   info.type = nir->info.stage;
   info.optLevel = 4;
   info.bin.nir = nir;
   info.io.genUserClip = -1;
   info.io.auxCBSlot = 15;
   info.io.msInfoCBSlot = 15;
   info.io.texBindBase = 0x20;
   info.assignSlots = assign_slots;
   struct nv50_ir_prog_info_out out = {0};
   int result = nv50_ir_generate_code(&info, &out);
   if (result || !out.bin.code || !out.bin.codeSize || out.bin.codeSize > 0xf80 ||
       out.bin.tlsSpace || out.bin.smemSize || out.bin.relocData || out.io.globalAccess ||
       out.io.fp64 || out.numBarriers || out.loops) {
      fprintf(stderr, "unsupported/failed %s shader: result=%d code=%u tls=%u smem=%u relocs=%p globals=%u fp64=%u sysvals=%u barriers=%u loops=%u\n",
              name, result, out.bin.codeSize, out.bin.tlsSpace, out.bin.smemSize,
              out.bin.relocData, out.io.globalAccess, out.io.fp64, out.numSysVals,
              out.numBarriers, out.loops);
      nv50_ir_prog_info_out_print(&out);
      exit(2);
   }
   for (unsigned i = 0; i < out.numSysVals; i++) {
      if (out.type != MESA_SHADER_FRAGMENT || out.sv[i].sn != SYSTEM_VALUE_BARYCENTRIC_PERSP_PIXEL) {
         fprintf(stderr, "unsupported shader system value %u\n", out.sv[i].sn); exit(2);
      }
   }
   if (out.bin.fixupData)
      nv50_ir_apply_fixups(out.bin.fixupData, out.bin.code, false, false, 0, false);
   uint32_t header[20]; make_header(&out, header);
   write_blob(directory, name, ".bin", out.bin.code, out.bin.codeSize);
   write_blob(directory, name, ".header.bin", header, sizeof(header));
   fprintf(metadata, "    {\"name\":\"%s\",\"stage\":\"%s\",\"binary_bytes\":%u,"
           "\"instructions\":%u,\"gprs\":%d,\"header_offset\":48,\"code_offset\":128,"
           "\"tls_bytes\":0,\"aux_cb_slot\":15,\"texture_handle_offset\":32,\"header\":[",
           name, out.type == MESA_SHADER_VERTEX ? "vertex" : "fragment",
           out.bin.codeSize, out.bin.instructions, out.bin.maxGPR + 1 < 4 ? 4 : out.bin.maxGPR + 1);
   for (unsigned i = 0; i < 20; i++) fprintf(metadata, "%s%u", i ? "," : "", header[i]);
   fprintf(metadata, "],\"inputs\":[");
   for (unsigned i = 0; i < out.numInputs; i++) {
      const struct nv50_ir_varying *v = &out.in[i];
      fprintf(metadata, "%s{\"semantic\":%u,\"index\":%u,\"mask\":%u,\"slots\":[%u,%u,%u,%u]}",
              i ? "," : "", v->sn, v->si, v->mask, v->slot[0], v->slot[1], v->slot[2], v->slot[3]);
   }
   fprintf(metadata, "],\"outputs\":[");
   for (unsigned i = 0; i < out.numOutputs; i++) {
      const struct nv50_ir_varying *v = &out.out[i];
      fprintf(metadata, "%s{\"semantic\":%u,\"index\":%u,\"mask\":%u,\"slots\":[%u,%u,%u,%u]}",
              i ? "," : "", v->sn, v->si, v->mask, v->slot[0], v->slot[1], v->slot[2], v->slot[3]);
   }
   fprintf(metadata, "]}%s\n", last ? "" : ",");
   free(out.bin.code);
   free(out.bin.fixupData);
   ralloc_free(nir);
}

int
main(int argc, char **argv)
{
   if (argc != 2) { fprintf(stderr, "usage: %s OUTPUT_DIR\n", argv[0]); return 2; }
   mkdir(argv[1], 0755);
   glsl_type_singleton_init_or_ref();
   char path[1024]; snprintf(path, sizeof(path), "%s/mesa-metadata.json", argv[1]);
   FILE *f = fopen(path, "w"); if (!f) { perror(path); return 2; }
   fprintf(f, "{\n  \"schema_version\":1,\n"
              "  \"mesa_sha\":\"e881540692daac6532cefec76699f7a025563767\",\n"
              "  \"chipset\":299,\n  \"uniform_bytes\":80,\n  \"variants\":[\n");
#define VS(name, p2, color, c3, uv) compile_one(argv[1], f, name, build_vs(name, p2, color, c3, uv), false)
#define FS(name, kind, last) compile_one(argv[1], f, name, build_fs(name, kind), last)
   VS("vs_stride16_pos2", true, false, false, false);
   VS("vs_stride16_pos2_uv2", true, false, false, true);
   VS("vs_stride40_pos4", false, false, false, false);
   VS("vs_stride40_pos4_color4", false, true, false, false);
   VS("vs_stride40_pos4_color4_uv2", false, true, false, true);
   VS("vs_stride24_pos4_uv2", false, false, false, true);
   VS("vs_stride28_pos4_color3", false, true, true, false);
   FS("fs_solid", FS_SOLID, false);
   FS("fs_vertex_color", FS_VERTEX_COLOR, false);
   FS("fs_texture_rgba", FS_TEXTURE_RGBA, false);
   FS("fs_texture_alpha_mask", FS_TEXTURE_ALPHA_MASK, false);
   FS("fs_texture_vertex_color_rgba", FS_TEXTURE_VERTEX_COLOR_RGBA, false);
   FS("fs_texture_rgb_ignore_alpha", FS_TEXTURE_RGB_IGNORE_ALPHA, true);
   fprintf(f, "  ]\n}\n"); fclose(f);
   glsl_type_singleton_decref();
   return 0;
}
