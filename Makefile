CC = clang
CFLAGS = -Wall -Wextra -O3 -std=gnu11
LDFLAGS = 

TARGET = utop_c

all: $(TARGET)

$(TARGET): utop.c
	$(CC) $(CFLAGS) utop.c -o $(TARGET) $(LDFLAGS)

clean:
	rm -f $(TARGET)
