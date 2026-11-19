import lzma
import struct
import sys


def unpack(source, destination):
    magic, chunk_size = struct.unpack(">4sQ", source.read(12))
    if magic != b"pbzx":
        raise ValueError("Expected a PBZX payload")

    while header := source.read(16):
        unpacked_size, stored_size = struct.unpack(">QQ", header)
        if not 0 < stored_size <= unpacked_size <= chunk_size:
            raise ValueError("Invalid PBZX chunk size")

        chunk = source.read(stored_size)
        if len(chunk) != stored_size:
            raise ValueError("Truncated PBZX chunk")
        if stored_size < unpacked_size:
            chunk = lzma.decompress(chunk, format=lzma.FORMAT_XZ)
        if len(chunk) != unpacked_size:
            raise ValueError("Incorrect PBZX uncompressed size")

        destination.write(chunk)


if __name__ == "__main__":
    unpack(sys.stdin.buffer, sys.stdout.buffer)
