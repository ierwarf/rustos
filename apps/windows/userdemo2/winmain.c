#include <stdio.h>

int main()
{
    while (1)
    {
        char asdf[10];

        if (scanf("%9s", asdf) != 1) {
            return 0;
        }

        printf("hi, %s\n\n", asdf);

        int a = 0;
        if (scanf("%d", &a) != 1) {
            return 0;
        }

        int b = 0;
        if (scanf("%d", &b) != 1) {
            return 0;
        }

        printf("%d\n\n", a + b);
    }
    return 0;
}
