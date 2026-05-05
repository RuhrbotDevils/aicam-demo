CXX = g++
CXXFLAGS = -shared -fPIC -O2 -std=c++17 \
	$(shell pkg-config --cflags gstreamer-1.0) \
	-I/usr/include/hailo/tappas \
	-I/usr/include/hailo \
	-I/usr/include/gstreamer-1.0/gst/hailo

TARGETS = libyolo26_post.so libmetadata_export.so

.PHONY: all clean

all: $(TARGETS)

libyolo26_post.so: yolo26_postprocess.cpp
	$(CXX) $(CXXFLAGS) -o $@ $<

# Metadata extraction filter. Links libzmq for publishing
# ObjectDetectionsMessage + RobotAttributesMessage to the ZMQ bus.
libmetadata_export.so: metadata_export.cpp
	$(CXX) $(CXXFLAGS) -o $@ $< -lzmq

clean:
	rm -f $(TARGETS)
