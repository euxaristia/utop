CC = clang
CFLAGS = -Wall -Wextra -O3 -std=gnu11
LDFLAGS = 

TARGET = utop

all: $(TARGET)

$(TARGET): src/main.c
	$(CC) $(CFLAGS) src/main.c -o $(TARGET) $(LDFLAGS)

clean:
	rm -f $(TARGET)
