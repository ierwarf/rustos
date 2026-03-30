#include <stdio.h>

int main()
{
    while (1)
    {
        char asdf[10];

        scanf("%s", asdf);

        printf("hi, %s\n\n", asdf);

        int a = 0;
        scanf("%d", &a);

        int b = 0;
        scanf("%d", &b);

        printf("%d\n\n", a + b);
    }
    return 0;
}
