/*
 * hello_rect.c — phase-3 milestone: render a colored rectangle on
 * the host's macOS window via the Fresco slot graph on FreeBSD.
 *
 * Pipeline:
 *   1. Build vertex blob (4 verts of a quad, NDC) → CAS upload
 *   2. Build index blob  (6 indices)              → CAS upload
 *   3. Build mesh blob   (refs verts+indices)     → CAS upload
 *   4. Build solid-color material blob            → CAS upload
 *   5. Build renderable blob (refs mesh+material) → CAS upload
 *   6. Allocate slot 1, set identity transform, content=renderable
 *   7. Set slot 1 as root
 *   8. FRAME_BEGIN / FRAME_END to commit
 *   9. Sleep 5 s so user can see the result
 *
 * If you see an orange rectangle on the host, the entire FreeBSD
 * Fresco stack is working end-to-end.
 */

#include <stdio.h>
#include <unistd.h>

#include "fresco.h"

#define DIE(...) do { fprintf(stderr, __VA_ARGS__); fresco_close(f); return 1; } while (0)

int
main(void)
{
        fresco_t *f = fresco_open(NULL);
        if (f == NULL) { perror("fresco_open"); return 1; }

        fresco_display_t disp;
        fresco_get_display(f, &disp);
        printf("display: %ux%u\n", disp.width, disp.height);

        /* 4 vertices of a quad in clip-space-ish coordinates. */
        float verts[12] = {
            -0.5f, -0.5f, 0.0f,
             0.5f, -0.5f, 0.0f,
             0.5f,  0.5f, 0.0f,
            -0.5f,  0.5f, 0.0f,
        };
        uint16_t indices[6] = { 0, 1, 2, 0, 2, 3 };

        uint8_t buf[256];
        fresco_hash_t vert_h, idx_h, mesh_h, mat_h, rend_h;

        size_t n = fresco_blob_vertex_data(buf, verts, 12);
        if (fresco_cas_put(f, buf, n, vert_h) != 0)
                DIE("cas_put vertex: %m\n");

        n = fresco_blob_index_data(buf, indices, 6);
        if (fresco_cas_put(f, buf, n, idx_h) != 0)
                DIE("cas_put index: %m\n");

        /* flags=0x0100 → POSITION-only vertex format (3 floats per vert). */
        n = fresco_blob_mesh(buf, /*verts*/4, /*indices*/6, 0x0100, vert_h, idx_h);
        if (fresco_cas_put(f, buf, n, mesh_h) != 0)
                DIE("cas_put mesh: %m\n");

        n = fresco_blob_material_solid(buf, 1.0f, 0.5f, 0.0f, 1.0f);  /* orange */
        if (fresco_cas_put(f, buf, n, mat_h) != 0)
                DIE("cas_put material: %m\n");

        n = fresco_blob_renderable(buf, mesh_h, mat_h);
        if (fresco_cas_put(f, buf, n, rend_h) != 0)
                DIE("cas_put renderable: %m\n");

        /* On-axis camera. The renderer's default is (0,2,5)→(0,0,0)
         * with 45° FOV — that produces a tilted, foreshortened view.
         * Place the camera at (0,0,5) facing -z, no tilt; the
         * camera-to-world matrix is just translate(0,0,5) in the
         * server's row-major convention. */
        float cam_xform[16] = {
            1, 0, 0, 0,
            0, 1, 0, 0,
            0, 0, 1, 0,
            0, 0, 5, 1,
        };
        fresco_hash_t cam_xform_h, cam_h;
        n = fresco_blob_transform(buf, cam_xform);
        if (fresco_cas_put(f, buf, n, cam_xform_h) != 0)
                DIE("cas_put cam_xform: %m\n");
        float aspect = (float)disp.width / (float)disp.height;
        n = fresco_blob_camera(buf, 0.7853982f, aspect, 0.1f, 100.0f, cam_xform_h);
        if (fresco_cas_put(f, buf, n, cam_h) != 0)
                DIE("cas_put camera: %m\n");
        if (fresco_set_camera(f, cam_h) != 0)
                DIE("set_camera: %m\n");

        printf("blobs uploaded:\n");
        printf("  verts %02x%02x..  idx %02x%02x..\n",
               vert_h[0], vert_h[1], idx_h[0], idx_h[1]);
        printf("  mesh  %02x%02x..  mat %02x%02x..  rend %02x%02x..\n",
               mesh_h[0], mesh_h[1], mat_h[0], mat_h[1],
               rend_h[0], rend_h[1]);
        printf("  cam   %02x%02x..  view  %02x%02x..\n",
               cam_h[0], cam_h[1], cam_xform_h[0], cam_xform_h[1]);

        /* Build the slot tree. */
        fresco_slot_t my_slot = 1;
        if (fresco_slot_alloc(f, my_slot, FRESCO_NODE_RENDERABLE,
                              FRESCO_SLOT_FLAG_VISIBLE) != 0)
                DIE("slot_alloc: %m\n");

        float ident[16];
        fresco_matrix_identity(ident);
        if (fresco_slot_set_xform_inline(f, my_slot, ident) != 0)
                DIE("slot_set_xform: %m\n");
        if (fresco_slot_set_content(f, my_slot, rend_h) != 0)
                DIE("slot_set_content: %m\n");
        if (fresco_slot_set_root(f, my_slot) != 0)
                DIE("slot_set_root: %m\n");

        /* Commit the frame. */
        fresco_frame_begin(f, 0);
        fresco_frame_end(f);

        printf("scene committed — sleeping 5 s. Look at the Fresco window!\n");
        sleep(5);

        fresco_close(f);
        printf("OK\n");
        return 0;
}
