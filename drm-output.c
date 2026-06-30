#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/mman.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <signal.h>
#include <xf86drm.h>
#include <xf86drmMode.h>

static volatile int running = 1;

static void sighandler(int sig) {
    (void)sig;
    running = 0;
}

static const char *connector_type_name(uint32_t type) {
    switch (type) {
        case DRM_MODE_CONNECTOR_VGA:         return "VGA";
        case DRM_MODE_CONNECTOR_DVII:        return "DVI-I";
        case DRM_MODE_CONNECTOR_DVID:        return "DVI-D";
        case DRM_MODE_CONNECTOR_DVIA:        return "DVI-A";
        case DRM_MODE_CONNECTOR_Composite:   return "Composite";
        case DRM_MODE_CONNECTOR_SVIDEO:      return "SVIDEO";
        case DRM_MODE_CONNECTOR_LVDS:        return "LVDS";
        case DRM_MODE_CONNECTOR_Component:   return "Component";
        case DRM_MODE_CONNECTOR_9PinDIN:     return "DIN";
        case DRM_MODE_CONNECTOR_DisplayPort: return "DP";
        case DRM_MODE_CONNECTOR_HDMIA:       return "HDMI-A";
        case DRM_MODE_CONNECTOR_HDMIB:       return "HDMI-B";
        case DRM_MODE_CONNECTOR_TV:          return "TV";
        case DRM_MODE_CONNECTOR_eDP:         return "eDP";
        case DRM_MODE_CONNECTOR_VIRTUAL:     return "Virtual";
        case DRM_MODE_CONNECTOR_DSI:         return "DSI";
        case DRM_MODE_CONNECTOR_DPI:         return "DPI";
        default:                             return "Unknown";
    }
}

int main(int argc, char *argv[]) {
    const char *target_connector = argc > 1 ? argv[1] : "DP-1";
    const char *shm_path = argc > 2 ? argv[2] : "/tmp/fbhalf-ext";
    const char *drm_device = argc > 3 ? argv[3] : "/dev/dri/card1";

    signal(SIGINT, sighandler);
    signal(SIGTERM, sighandler);

    int drm_fd = open(drm_device, O_RDWR);
    if (drm_fd < 0) {
        perror("open drm device");
        return 1;
    }

    /* Try to become DRM master */
    drmSetMaster(drm_fd);

    drmModeRes *res = drmModeGetResources(drm_fd);
    if (!res) {
        fprintf(stderr, "drmModeGetResources failed (need root?)\n");
        close(drm_fd);
        return 1;
    }

    /* Find the target connector */
    drmModeConnector *conn = NULL;
    for (int i = 0; i < res->count_connectors; i++) {
        drmModeConnector *c = drmModeGetConnector(drm_fd, res->connectors[i]);
        if (!c) continue;

        char name[64];
        snprintf(name, sizeof(name), "%s-%u",
                 connector_type_name(c->connector_type),
                 c->connector_type_id);

        if (strcmp(name, target_connector) == 0 &&
            c->connection == DRM_MODE_CONNECTED) {
            conn = c;
            break;
        }
        drmModeFreeConnector(c);
    }

    if (!conn || conn->count_modes == 0) {
        fprintf(stderr, "Connector %s not found or has no modes\n", target_connector);
        drmModeFreeResources(res);
        close(drm_fd);
        return 1;
    }

    /* Use first (preferred) mode */
    drmModeModeInfo mode = conn->modes[0];

    /* Find CRTC */
    uint32_t crtc_id = 0;
    if (conn->encoder_id) {
        drmModeEncoder *enc = drmModeGetEncoder(drm_fd, conn->encoder_id);
        if (enc) {
            crtc_id = enc->crtc_id;
            drmModeFreeEncoder(enc);
        }
    }
    if (!crtc_id) {
        for (int i = 0; i < conn->count_encoders; i++) {
            drmModeEncoder *enc = drmModeGetEncoder(drm_fd, conn->encoders[i]);
            if (!enc) continue;
            for (int j = 0; j < res->count_crtcs; j++) {
                if (enc->possible_crtcs & (1u << j)) {
                    crtc_id = res->crtcs[j];
                    break;
                }
            }
            drmModeFreeEncoder(enc);
            if (crtc_id) break;
        }
    }

    if (!crtc_id) {
        fprintf(stderr, "No CRTC found for %s\n", target_connector);
        drmModeFreeConnector(conn);
        drmModeFreeResources(res);
        close(drm_fd);
        return 1;
    }

    /* Create dumb buffer */
    struct drm_mode_create_dumb create = {0};
    create.width = mode.hdisplay;
    create.height = mode.vdisplay;
    create.bpp = 32;

    if (ioctl(drm_fd, DRM_IOCTL_MODE_CREATE_DUMB, &create) < 0) {
        perror("CREATE_DUMB");
        drmModeFreeConnector(conn);
        drmModeFreeResources(res);
        close(drm_fd);
        return 1;
    }

    /* Add framebuffer */
    uint32_t fb_id;
    if (drmModeAddFB(drm_fd, mode.hdisplay, mode.vdisplay,
                     24, 32, create.pitch, create.handle, &fb_id)) {
        perror("AddFB");
        close(drm_fd);
        return 1;
    }

    /* Map the buffer */
    struct drm_mode_map_dumb map = {0};
    map.handle = create.handle;
    if (ioctl(drm_fd, DRM_IOCTL_MODE_MAP_DUMB, &map) < 0) {
        perror("MAP_DUMB");
        close(drm_fd);
        return 1;
    }

    uint8_t *fb_ptr = mmap(NULL, create.size, PROT_READ | PROT_WRITE,
                           MAP_SHARED, drm_fd, map.offset);
    if (fb_ptr == MAP_FAILED) {
        perror("mmap drm");
        close(drm_fd);
        return 1;
    }

    /* Save old CRTC for restoration */
    drmModeCrtc *old_crtc = drmModeGetCrtc(drm_fd, crtc_id);

    /* Set CRTC */
    if (drmModeSetCrtc(drm_fd, crtc_id, fb_id, 0, 0,
                       &conn->connector_id, 1, &mode)) {
        perror("SetCrtc");
        munmap(fb_ptr, create.size);
        close(drm_fd);
        return 1;
    }

    /* Create shared memory file */
    umask(0);
    int shm_fd = open(shm_path, O_RDWR | O_CREAT, 0666);
    if (shm_fd < 0) {
        perror("open shm");
        munmap(fb_ptr, create.size);
        close(drm_fd);
        return 1;
    }
    if (ftruncate(shm_fd, create.size) < 0) {
        perror("ftruncate");
        close(shm_fd);
        munmap(fb_ptr, create.size);
        close(drm_fd);
        return 1;
    }

    uint8_t *shm_ptr = mmap(NULL, create.size, PROT_READ,
                            MAP_SHARED, shm_fd, 0);
    if (shm_ptr == MAP_FAILED) {
        perror("mmap shm");
        close(shm_fd);
        munmap(fb_ptr, create.size);
        close(drm_fd);
        return 1;
    }

    /* Write info file for ssbrowse to read */
    char info_path[256];
    snprintf(info_path, sizeof(info_path), "%s.info", shm_path);
    FILE *info_fp = fopen(info_path, "w");
    if (info_fp) {
        fprintf(info_fp, "%u %u %u %llu\n",
                mode.hdisplay, mode.vdisplay, create.pitch,
                (unsigned long long)create.size);
        fclose(info_fp);
        chmod(info_path, 0666);
    }

    printf("READY %ux%u pitch=%u size=%llu\n",
           mode.hdisplay, mode.vdisplay, create.pitch,
           (unsigned long long)create.size);
    printf("shm: %s\ninfo: %s\n", shm_path, info_path);
    fflush(stdout);

    /* Main loop: copy shm -> DRM buffer */
    while (running) {
        memcpy(fb_ptr, shm_ptr, create.size);
        usleep(16000);  /* ~60fps */
    }

    /* Cleanup */
    fprintf(stderr, "drm-output: cleaning up\n");

    if (old_crtc) {
        drmModeSetCrtc(drm_fd, old_crtc->crtc_id, old_crtc->buffer_id,
                       old_crtc->x, old_crtc->y,
                       &conn->connector_id, 1, &old_crtc->mode);
        drmModeFreeCrtc(old_crtc);
    }

    munmap(shm_ptr, create.size);
    close(shm_fd);
    unlink(shm_path);
    unlink(info_path);
    munmap(fb_ptr, create.size);
    drmModeFreeConnector(conn);
    drmModeFreeResources(res);

    struct drm_mode_destroy_dumb destroy = {0};
    destroy.handle = create.handle;
    ioctl(drm_fd, DRM_IOCTL_MODE_DESTROY_DUMB, &destroy);

    close(drm_fd);
    return 0;
}
