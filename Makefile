fbhalf-drm-output: drm-output.c
	gcc -O2 -Wall -I/usr/include/libdrm -o fbhalf-drm-output drm-output.c -ldrm

clean:
	rm -f fbhalf-drm-output

.PHONY: clean
